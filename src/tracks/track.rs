//! Individual track representation

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::audio::buffer::{create_shared_buffer, SharedRingBuffer};
use crate::config::OpusConfig;
use crate::error::TrackError;
use crate::protocol::{TrackConfig, TrackStatus, TrackType};
use crate::constants::RING_BUFFER_CAPACITY;

/// Track state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    /// Track is created but not started
    Stopped,
    /// Track is starting
    Starting,
    /// Track is running
    Running,
    /// Track is stopping
    Stopping,
    /// Track encountered an error
    Error,
}

/// Audio track (sender or receiver)
/// 
/// Note: Encoders/decoders are NOT stored in Track to maintain thread safety.
/// They should be created and managed separately in the audio processing pipeline.
pub struct Track {
    /// Track ID
    pub id: u8,
    
    /// Human-readable name
    pub name: String,
    
    /// Device ID (input for sender, output for receiver)
    pub device_id: String,
    
    /// Track configuration
    pub config: TrackConfig,
    
    /// Current state
    state: TrackState,
    
    /// Muted flag
    muted: Arc<AtomicBool>,
    
    /// Solo flag
    solo: Arc<AtomicBool>,
    
    /// Audio buffer
    pub buffer: SharedRingBuffer,
    
    /// Packets sent/received
    packets_count: Arc<AtomicU64>,
    
    /// Packets lost
    packets_lost: Arc<AtomicU64>,
    
    /// Current latency in microseconds (stored as AtomicU32 for thread safety)
    latency_us: Arc<AtomicU32>,
    
    /// Current jitter estimate in microseconds (stored as AtomicU32 for thread safety)
    jitter_us: Arc<AtomicU32>,
    
    /// Start time
    start_time: Option<Instant>,
    
    /// Last error message
    last_error: Option<String>,
    
    /// Peak level (dB) - stored as AtomicU32 in millibels for thread safety
    peak_level_millibels: Arc<AtomicU32>,
}

// Track is now Send + Sync safe (no raw pointers)
unsafe impl Send for Track {}
unsafe impl Sync for Track {}

impl Track {
    /// Create a new track
    pub fn new(id: u8, config: TrackConfig) -> Self {
        Self {
            id,
            name: config.name.clone(),
            device_id: config.device_id.clone(),
            config,
            state: TrackState::Stopped,
            muted: Arc::new(AtomicBool::new(false)),
            solo: Arc::new(AtomicBool::new(false)),
            buffer: create_shared_buffer(RING_BUFFER_CAPACITY),
            packets_count: Arc::new(AtomicU64::new(0)),
            packets_lost: Arc::new(AtomicU64::new(0)),
            latency_us: Arc::new(AtomicU32::new(0)),
            jitter_us: Arc::new(AtomicU32::new(0)),
            start_time: None,
            last_error: None,
            peak_level_millibels: Arc::new(AtomicU32::new(0)), // 0 represents -96.0 dB
        }
    }
    
    /// Create Opus config from track config
    pub fn create_opus_config(&self) -> OpusConfig {
        let frame_size = OpusConfig::frame_size_from_ms(
            48000, // Assuming 48kHz
            self.config.frame_size_ms,
        );
        
        let base_config = match self.config.track_type {
            TrackType::Voice => OpusConfig::voice(),
            TrackType::Music => OpusConfig::music(),
            TrackType::LowLatency => OpusConfig::low_latency(),
        };
        
        OpusConfig {
            bitrate: self.config.bitrate,
            frame_size,
            channels: self.config.channels,
            fec: self.config.fec_enabled,
            ..base_config
        }
    }
    
    /// Start the track
    pub fn start(&mut self) -> Result<(), TrackError> {
        if self.state == TrackState::Running {
            return Ok(());
        }
        
        self.state = TrackState::Starting;
        self.start_time = Some(Instant::now());
        self.packets_count.store(0, Ordering::Relaxed);
        self.packets_lost.store(0, Ordering::Relaxed);
        self.state = TrackState::Running;
        
        Ok(())
    }
    
    /// Stop the track
    pub fn stop(&mut self) {
        self.state = TrackState::Stopping;
        self.start_time = None;
        self.state = TrackState::Stopped;
    }
    
    /// Get current state
    pub fn state(&self) -> TrackState {
        self.state
    }
    
    /// Set state (internal use)
    pub fn set_state(&mut self, state: TrackState) {
        self.state = state;
    }
    
    /// Check if running
    pub fn is_running(&self) -> bool {
        self.state == TrackState::Running
    }
    
    /// Set muted state
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
    
    /// Get muted state
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    
    /// Set solo state
    pub fn set_solo(&self, solo: bool) {
        self.solo.store(solo, Ordering::Relaxed);
    }
    
    /// Get solo state
    pub fn is_solo(&self) -> bool {
        self.solo.load(Ordering::Relaxed)
    }
    
    /// Increment packet count
    pub fn increment_packets(&self) {
        self.packets_count.fetch_add(1, Ordering::Relaxed);
    }
    
    /// Increment lost packet count
    pub fn increment_lost(&self) {
        self.packets_lost.fetch_add(1, Ordering::Relaxed);
    }
    
    /// Get packet count
    pub fn packets_count(&self) -> u64 {
        self.packets_count.load(Ordering::Relaxed)
    }
    
    /// Get lost packet count
    pub fn packets_lost(&self) -> u64 {
        self.packets_lost.load(Ordering::Relaxed)
    }
    
    /// Update peak level from samples
    pub fn update_level(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        
        let peak = samples.iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        
        // Convert to dB
        let db = if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            -96.0
        };
        
        // Get current level
        let current_millibels = self.peak_level_millibels.load(Ordering::Relaxed);
        let current_db = (current_millibels as f32 / 100.0) - 96.0;
        
        // Smooth the level (simple IIR filter)
        let new_db = current_db * 0.9 + db * 0.1;
        
        // Store as millibels (add 96 to make positive, multiply by 100 for precision)
        let new_millibels = ((new_db + 96.0) * 100.0) as u32;
        self.peak_level_millibels.store(new_millibels, Ordering::Relaxed);
    }
    
    /// Thread-safe update of peak level
    pub fn update_level_atomic(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        
        let peak = samples.iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        
        // Convert to dB
        let db = if peak > 0.0 {
            20.0 * peak.log10()
        } else {
            -96.0
        };
        
        // Get current level
        let current_millibels = self.peak_level_millibels.load(Ordering::Relaxed);
        let current_db = (current_millibels as f32 / 100.0) - 96.0;
        
        // Smooth the level (simple IIR filter)
        let new_db = current_db * 0.9 + db * 0.1;
        
        // Store as millibels (add 96 to make positive, multiply by 100 for precision)
        let new_millibels = ((new_db + 96.0) * 100.0) as u32;
        self.peak_level_millibels.store(new_millibels, Ordering::Relaxed);
    }
    
    /// Get current level in dB
    pub fn level_db(&self) -> f32 {
        let millibels = self.peak_level_millibels.load(Ordering::Relaxed);
        (millibels as f32 / 100.0) - 96.0
    }
    
    /// Update latency measurement (in microseconds)
    pub fn update_latency(&self, latency_us: u32) {
        self.latency_us.store(latency_us, Ordering::Relaxed);
    }
    
    /// Get current latency in milliseconds
    pub fn latency_ms(&self) -> f32 {
        self.latency_us.load(Ordering::Relaxed) as f32 / 1000.0
    }
    
    /// Update jitter measurement (in microseconds)
    pub fn update_jitter(&self, jitter_us: u32) {
        self.jitter_us.store(jitter_us, Ordering::Relaxed);
    }
    
    /// Get current jitter in milliseconds
    pub fn jitter_ms(&self) -> f32 {
        self.jitter_us.load(Ordering::Relaxed) as f32 / 1000.0
    }
    
    /// Set error state
    pub fn set_error(&mut self, error: String) {
        self.state = TrackState::Error;
        self.last_error = Some(error);
    }
    
    /// Get last error
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    
    /// Update configuration
    pub fn update_config(&mut self, update: &crate::protocol::TrackConfigUpdate) -> Result<(), TrackError> {
        if let Some(ref name) = update.name {
            self.name = name.clone();
            self.config.name = name.clone();
        }
        
        if let Some(ref device_id) = update.device_id {
            self.device_id = device_id.clone();
            self.config.device_id = device_id.clone();
        }
        
        if let Some(bitrate) = update.bitrate {
            self.config.bitrate = bitrate;
            // Note: If encoder exists elsewhere, caller needs to update it
        }
        
        if let Some(frame_size_ms) = update.frame_size_ms {
            self.config.frame_size_ms = frame_size_ms;
            // Note: Frame size change requires encoder recreation
        }
        
        if let Some(fec) = update.fec_enabled {
            self.config.fec_enabled = fec;
            // Note: If encoder exists elsewhere, caller needs to update it
        }
        
        Ok(())
    }
    
    /// Get track status for reporting
    pub fn status(&self) -> TrackStatus {
        TrackStatus {
            track_id: self.id,
            name: self.name.clone(),
            device_id: self.device_id.clone(),
            active: self.is_running(),
            muted: self.is_muted(),
            solo: self.is_solo(),
            bitrate: self.config.bitrate,
            frame_size_ms: self.config.frame_size_ms,
            packets_sent: self.packets_count(),
            packets_received: self.packets_count(),
            packets_lost: self.packets_lost(),
            current_latency_ms: self.latency_ms(),
            jitter_ms: self.jitter_ms(),
            level_db: self.level_db(),
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        self.stop();
    }
}
