//! Audio playback to output devices
//!
//! Handles playing back decoded audio to output devices,
//! with support for virtual audio devices for OBS integration.

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::StreamConfig;
use crossbeam_channel::{bounded, Receiver};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::audio::buffer::{AudioFrame, JitterBuffer, SharedRingBuffer};
use crate::audio::device::get_device_by_id;
use crate::constants::DEFAULT_SAMPLE_RATE;
use crate::error::AudioError;

/// Audio playback instance for a single device/track
pub struct AudioPlayback {
    /// Track ID this playback belongs to
    track_id: u8,
    
    /// Device identifier
    device_id: String,
    
    /// Whether playback is running
    running: Arc<AtomicBool>,
    
    /// Input buffer for frames to play
    input_buffer: SharedRingBuffer,
    
    /// Stream thread handle
    thread_handle: Option<JoinHandle<()>>,
    
    /// Channel for stream errors
    error_rx: Option<Receiver<AudioError>>,
    
    /// Total samples played
    samples_played: Arc<AtomicU64>,
    
    /// Buffer underruns
    underruns: Arc<AtomicU32>,
    
    /// Stream configuration
    config: StreamConfig,
    
    /// Muted state
    muted: Arc<AtomicBool>,
    
    /// Volume (0.0 - 1.0)
    volume: Arc<parking_lot::RwLock<f32>>,
}

impl AudioPlayback {
    /// Create a new audio playback for the specified device
    pub fn new(
        track_id: u8,
        device_id: &str,
        sample_rate: Option<u32>,
        channels: Option<u16>,
        buffer_size: Option<u32>,
        input_buffer: SharedRingBuffer,
    ) -> Result<Self, AudioError> {
        let device = get_device_by_id(device_id)?;
        
        // Get default config and override with requested settings
        let default_config = device.default_output_config()?;
        
        let config = StreamConfig {
            channels: channels.unwrap_or(default_config.channels()),
            sample_rate: cpal::SampleRate(sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE)),
            buffer_size: match buffer_size {
                Some(size) => cpal::BufferSize::Fixed(size),
                None => cpal::BufferSize::Default,
            },
        };
        
        Ok(Self {
            track_id,
            device_id: device_id.to_string(),
            running: Arc::new(AtomicBool::new(false)),
            input_buffer,
            thread_handle: None,
            error_rx: None,
            samples_played: Arc::new(AtomicU64::new(0)),
            underruns: Arc::new(AtomicU32::new(0)),
            config,
            muted: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(parking_lot::RwLock::new(1.0)),
        })
    }
    
    /// Start playback
    pub fn start(&mut self) -> Result<(), AudioError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        let device = get_device_by_id(&self.device_id)?;
        let (error_tx, error_rx) = bounded::<AudioError>(16);
        self.error_rx = Some(error_rx);
        
        let running = self.running.clone();
        let running_for_loop = self.running.clone();
        let input_buffer = self.input_buffer.clone();
        let samples_played = self.samples_played.clone();
        let underruns = self.underruns.clone();
        let config = self.config.clone();
        let _channels = self.config.channels as usize;
        let muted = self.muted.clone();
        let volume = self.volume.clone();
        
        running.store(true, Ordering::SeqCst);
        
        let handle = thread::Builder::new()
            .name(format!("playback-track-{}", self.track_id))
            .spawn(move || {
                let cpal_device = device.into_inner();
                
                // Buffered samples for smooth playback
                let mut sample_buffer: Vec<f32> = Vec::new();
                let mut sample_pos = 0;
                
                let stream = cpal_device.build_output_stream(
                    &config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if !running.load(Ordering::Relaxed) {
                            // Fill with silence
                            for sample in data.iter_mut() {
                                *sample = 0.0;
                            }
                            return;
                        }
                        
                        let is_muted = muted.load(Ordering::Relaxed);
                        let vol = *volume.read();
                        
                        for sample in data.iter_mut() {
                            // Check if we need more samples
                            if sample_pos >= sample_buffer.len() {
                                // Try to get next frame
                                if let Some(frame) = input_buffer.try_pop() {
                                    sample_buffer = frame.samples;
                                    sample_pos = 0;
                                } else {
                                    // Underrun - output silence
                                    underruns.fetch_add(1, Ordering::Relaxed);
                                    *sample = 0.0;
                                    continue;
                                }
                            }
                            
                            // Output sample (with mute and volume)
                            if is_muted {
                                *sample = 0.0;
                            } else {
                                *sample = sample_buffer[sample_pos] * vol;
                            }
                            sample_pos += 1;
                        }
                        
                        samples_played.fetch_add(data.len() as u64, Ordering::Relaxed);
                    },
                    move |err| {
                        let _ = error_tx.try_send(AudioError::StreamError(err.to_string()));
                    },
                    None,
                );
                
                match stream {
                    Ok(stream) => {
                        if let Err(e) = stream.play() {
                            tracing::error!("Failed to start playback stream: {}", e);
                            return;
                        }
                        
                        // Keep thread alive while running
                        while running_for_loop.load(Ordering::Relaxed) {
                            thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to build playback stream: {}", e);
                    }
                }
            })
            .map_err(|e| AudioError::StreamError(e.to_string()))?;
        
        self.thread_handle = Some(handle);
        Ok(())
    }
    
    /// Stop playback
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
    
    /// Check if playback is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    
    /// Set mute state
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
    
    /// Get mute state
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    
    /// Set volume (0.0 - 1.0)
    pub fn set_volume(&self, volume: f32) {
        *self.volume.write() = volume.clamp(0.0, 1.0);
    }
    
    /// Get volume
    pub fn volume(&self) -> f32 {
        *self.volume.read()
    }
    
    /// Get total samples played
    pub fn samples_played(&self) -> u64 {
        self.samples_played.load(Ordering::Relaxed)
    }
    
    /// Get underrun count
    pub fn underruns(&self) -> u32 {
        self.underruns.load(Ordering::Relaxed)
    }
    
    /// Get the stream configuration
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }
    
    /// Get sample rate
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate.0
    }
    
    /// Get channel count
    pub fn channels(&self) -> u16 {
        self.config.channels
    }
    
    /// Check for errors
    pub fn check_errors(&self) -> Option<AudioError> {
        self.error_rx.as_ref().and_then(|rx| rx.try_recv().ok())
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Playback with jitter buffer for network audio
pub struct NetworkPlayback {
    /// Inner playback
    playback: AudioPlayback,
    
    /// Jitter buffer for reordering
    jitter_buffer: parking_lot::Mutex<JitterBuffer>,
    
    /// Decoded frame buffer
    decoded_buffer: SharedRingBuffer,
}

impl NetworkPlayback {
    /// Create network playback with jitter buffering
    pub fn new(
        track_id: u8,
        device_id: &str,
        sample_rate: Option<u32>,
        channels: Option<u16>,
        jitter_buffer_size: usize,
        min_jitter_delay: usize,
    ) -> Result<Self, AudioError> {
        let decoded_buffer = crate::audio::buffer::create_shared_buffer(64);
        
        let playback = AudioPlayback::new(
            track_id,
            device_id,
            sample_rate,
            channels,
            None,
            decoded_buffer.clone(),
        )?;
        
        let jitter_buffer = parking_lot::Mutex::new(JitterBuffer::new(
            jitter_buffer_size.next_power_of_two(),
            min_jitter_delay,
        ));
        
        Ok(Self {
            playback,
            jitter_buffer,
            decoded_buffer,
        })
    }
    
    /// Push a decoded frame to the jitter buffer
    pub fn push_frame(&self, frame: AudioFrame) -> bool {
        let mut jitter = self.jitter_buffer.lock();
        jitter.insert(frame)
    }
    
    /// Push a decoded frame directly to the output buffer (bypassing jitter buffer)
    /// Use this when you have your own jitter buffer management
    pub fn push_frame_direct(&self, frame: AudioFrame) -> bool {
        self.decoded_buffer.push(frame)
    }
    
    /// Process jitter buffer and push to playback
    pub fn process(&self) -> Option<AudioFrame> {
        let mut jitter = self.jitter_buffer.lock();
        if let Some(frame) = jitter.get_next() {
            let _ = self.decoded_buffer.push(frame.clone());
            Some(frame)
        } else {
            None
        }
    }
    
    /// Start playback
    pub fn start(&mut self) -> Result<(), AudioError> {
        self.playback.start()
    }
    
    /// Stop playback
    pub fn stop(&mut self) {
        self.playback.stop();
    }
    
    /// Get jitter buffer stats
    pub fn jitter_stats(&self) -> crate::audio::buffer::JitterBufferStats {
        self.jitter_buffer.lock().stats()
    }
    
    /// Get inner playback
    pub fn playback(&self) -> &AudioPlayback {
        &self.playback
    }
    
    /// Get mutable inner playback
    pub fn playback_mut(&mut self) -> &mut AudioPlayback {
        &mut self.playback
    }
}

/// Multi-track playback manager
pub struct MultiPlayback {
    playbacks: Vec<NetworkPlayback>,
}

impl MultiPlayback {
    pub fn new() -> Self {
        Self {
            playbacks: Vec::new(),
        }
    }
    
    /// Add a playback for a track
    pub fn add_playback(&mut self, playback: NetworkPlayback) {
        self.playbacks.push(playback);
    }
    
    /// Remove a playback by track ID
    pub fn remove_playback(&mut self, track_id: u8) -> Option<NetworkPlayback> {
        if let Some(pos) = self.playbacks.iter().position(|p| p.playback.track_id == track_id) {
            Some(self.playbacks.remove(pos))
        } else {
            None
        }
    }
    
    /// Start all playbacks
    pub fn start_all(&mut self) -> Result<(), AudioError> {
        for playback in &mut self.playbacks {
            playback.start()?;
        }
        Ok(())
    }
    
    /// Stop all playbacks
    pub fn stop_all(&mut self) {
        for playback in &mut self.playbacks {
            playback.stop();
        }
    }
    
    /// Get playback by track ID
    pub fn get_playback(&self, track_id: u8) -> Option<&NetworkPlayback> {
        self.playbacks.iter().find(|p| p.playback.track_id == track_id)
    }
    
    /// Get mutable playback by track ID
    pub fn get_playback_mut(&mut self, track_id: u8) -> Option<&mut NetworkPlayback> {
        self.playbacks.iter_mut().find(|p| p.playback.track_id == track_id)
    }
}

impl Default for MultiPlayback {
    fn default() -> Self {
        Self::new()
    }
}
