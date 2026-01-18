//! Audio Receiver Application
//!
//! Receives audio streams from sender and outputs to virtual devices.

use anyhow::Result;
use crossbeam_channel::bounded;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use lan_audio_streamer::{
    audio::{
        buffer::{AudioFrame, JitterBuffer},
        device::list_devices,
        playback::NetworkPlayback,
    },
    codec::OpusDecoder,
    config::AppConfig,
    constants::*,
    network::receiver::{AudioReceiver, ReceivedPacket},
    protocol::TrackConfig,
    tracks::{TrackManager, TrackEvent},
    ui::WebServer,
};

/// Per-track receiver state
struct TrackState {
    decoder: OpusDecoder,
    jitter_buffer: JitterBuffer,
    playback: Option<NetworkPlayback>,
    packets_received: u64,
    packets_lost: u64,
    device_id: String,
    channels: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();
    
    tracing::info!("Starting LAN Audio Receiver");
    
    // Load or create config
    let config = AppConfig::default();
    
    // List available output devices
    println!("\n=== Available Output Devices ===");
    let devices = list_devices();
    for device in &devices {
        if device.is_output {
            let default_marker = if device.is_default { " [DEFAULT]" } else { "" };
            println!("  {}{}:", device.name, default_marker);
            println!("    ID: {}", device.id);
            println!("    Sample rates: {:?}", device.sample_rates);
            println!("    Channels: {:?}", device.channels);
        }
    }
    println!();
    
    // Create track manager
    let track_manager = Arc::new(TrackManager::new());
    
    // Subscribe to track events BEFORE starting web UI
    let mut event_rx = track_manager.subscribe();
    
    // Start web UI
    let web_server = WebServer::new(
        config.ui.clone(),
        track_manager.clone(),
        false, // is_receiver
    );
    let _web_handle = web_server.start_background();
    
    tracing::info!("Web UI available at http://{}:{}", config.ui.bind_address, config.ui.http_port);
    
    // Create packet receiver channel
    let (packet_tx, packet_rx) = bounded::<ReceivedPacket>(4096);
    
    // Create and start network receiver
    let mut receiver = AudioReceiver::new();
    receiver.set_global_channel(packet_tx);
    receiver.start(config.network.clone())?;
    
    tracing::info!("Network receiver started on port {}", config.network.udp_port);
    
    // Track states - shared mutable map for runtime reconfiguration
    let track_states: Arc<Mutex<HashMap<u8, TrackState>>> = Arc::new(Mutex::new(HashMap::new()));
    let track_states_for_events = track_states.clone();
    
    // Set of manually deleted tracks - don't auto-recreate these
    let deleted_tracks: Arc<Mutex<HashSet<u8>>> = Arc::new(Mutex::new(HashSet::new()));
    let deleted_tracks_for_events = deleted_tracks.clone();
    
    // Get default output device
    let default_output = devices.iter()
        .find(|d| d.is_output && d.is_default)
        .map(|d| d.id.clone())
        .unwrap_or_default();
    
    tracing::info!("Default output device: {}", default_output);
    
    // Spawn task to handle track events (device changes)
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    match event {
                        TrackEvent::DeviceChanged(track_id, old_device, new_device) => {
                            tracing::info!(
                                "Track {} output device changed: {} -> {}",
                                track_id, old_device, new_device
                            );
                            
                            let mut states = track_states_for_events.lock();
                            if let Some(state) = states.get_mut(&track_id) {
                                // Stop old playback
                                if let Some(ref mut old_playback) = state.playback {
                                    old_playback.stop();
                                    tracing::info!("Stopped old playback for track {}", track_id);
                                }
                                
                                // Create new playback with new device
                                let channels = state.channels;
                                match NetworkPlayback::new(
                                    track_id,
                                    &new_device,
                                    Some(DEFAULT_SAMPLE_RATE),
                                    Some(channels),
                                    32, // jitter buffer size
                                    2,  // min delay
                                ) {
                                    Ok(mut p) => {
                                        if let Err(e) = p.start() {
                                            tracing::error!(
                                                "Failed to start playback for track {} on {}: {}",
                                                track_id, new_device, e
                                            );
                                            state.playback = None;
                                        } else {
                                            tracing::info!(
                                                "Successfully switched track {} to output device {}",
                                                track_id, new_device
                                            );
                                            state.playback = Some(p);
                                            state.device_id = new_device.clone();
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to create playback for track {} on {}: {}",
                                            track_id, new_device, e
                                        );
                                        state.playback = None;
                                    }
                                }
                            }
                        }
                        
                        TrackEvent::Removed(track_id) => {
                            tracing::info!("Track {} removed by user, stopping playback...", track_id);
                            
                            // Add to deleted set so it won't be auto-recreated
                            deleted_tracks_for_events.lock().insert(track_id);
                            
                            let mut states = track_states_for_events.lock();
                            if let Some(mut state) = states.remove(&track_id) {
                                if let Some(ref mut playback) = state.playback {
                                    playback.stop();
                                }
                                tracing::info!("Playback stopped for track {}", track_id);
                            }
                        }
                        
                        TrackEvent::Created(track_id) => {
                            // If user manually creates a track, remove from deleted set
                            deleted_tracks_for_events.lock().remove(&track_id);
                            tracing::info!("Track {} created by user", track_id);
                        }
                        
                        _ => {
                            // Other events
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Event channel error: {}", e);
                }
            }
        }
    });
    
    tracing::info!("Waiting for audio streams...");
    
    // Main receiving loop
    let mut last_stats_time = std::time::Instant::now();
    
    loop {
        // Process received packets
        while let Ok(packet) = packet_rx.try_recv() {
            let track_id = packet.track_id;
            
            // Skip packets for deleted tracks
            if deleted_tracks.lock().contains(&track_id) {
                continue;
            }
            
            let mut states = track_states.lock();
            
            // Initialize track state if new
            if !states.contains_key(&track_id) {
                tracing::info!("New track {} detected, initializing...", track_id);
                
                // Determine channel count from packet
                let channels = if packet.is_stereo { 2 } else { 1 };
                
                // Check if track already exists in manager (user may have pre-configured it)
                let output_device = if let Some(track) = track_manager.get_track(track_id) {
                    if !track.device_id.is_empty() {
                        track.device_id.clone()
                    } else {
                        default_output.clone()
                    }
                } else {
                    default_output.clone()
                };
                
                // Create decoder
                let frame_size = (DEFAULT_SAMPLE_RATE as f32 * DEFAULT_FRAME_SIZE_MS / 1000.0) as usize;
                let decoder = match OpusDecoder::new(DEFAULT_SAMPLE_RATE, channels, frame_size) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::error!("Failed to create decoder for track {}: {}", track_id, e);
                        continue;
                    }
                };
                
                // Create jitter buffer (32 slots, 2 frame minimum delay)
                let jitter_buffer = JitterBuffer::new(32, 2);
                
                // Create playback (optional - may not have output device)
                let playback = if !output_device.is_empty() {
                    match NetworkPlayback::new(
                        track_id,
                        &output_device,
                        Some(DEFAULT_SAMPLE_RATE),
                        Some(channels),
                        32, // jitter buffer size
                        2,  // min delay
                    ) {
                        Ok(mut p) => {
                            if let Err(e) = p.start() {
                                tracing::warn!("Failed to start playback for track {}: {}", track_id, e);
                                None
                            } else {
                                tracing::info!("Started playback for track {} on {}", track_id, output_device);
                                Some(p)
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to create playback for track {}: {}", track_id, e);
                            None
                        }
                    }
                } else {
                    None
                };
                
                // Create track in manager (only if it doesn't exist)
                if track_manager.get_track(track_id).is_none() {
                    let track_config = TrackConfig {
                        track_id: Some(track_id),
                        name: format!("Track {}", track_id),
                        device_id: output_device.clone(),
                        bitrate: DEFAULT_BITRATE,
                        frame_size_ms: DEFAULT_FRAME_SIZE_MS,
                        channels,
                        ..Default::default()
                    };
                    let _ = track_manager.create_track(track_config);
                }
                
                states.insert(track_id, TrackState {
                    decoder,
                    jitter_buffer,
                    playback,
                    packets_received: 0,
                    packets_lost: 0,
                    device_id: output_device.clone(),
                    channels,
                });
            }
            
            // Process packet
            if let Some(state) = states.get_mut(&track_id) {
                state.packets_received += 1;
                
                // Decode audio
                match state.decoder.decode(&packet.payload) {
                    Ok(samples) => {
                        // Create audio frame
                        let frame = AudioFrame::new(
                            samples,
                            state.decoder.channels(),
                            packet.timestamp,
                            packet.sequence,
                        );
                        
                        // Insert into jitter buffer for reordering
                        state.jitter_buffer.insert(frame);
                        
                        // Process jitter buffer and push ready frames to playback
                        // This handles packet reordering before sending to audio output
                        while let Some(ready_frame) = state.jitter_buffer.get_next() {
                            if let Some(ref playback) = state.playback {
                                playback.push_frame_direct(ready_frame);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Decode error on track {}: {}", track_id, e);
                        state.packets_lost += 1;
                    }
                }
            }
        }
        
        // Periodic stats
        if last_stats_time.elapsed() >= Duration::from_secs(5) {
            last_stats_time = std::time::Instant::now();
            
            let recv_stats = receiver.stats();
            tracing::info!(
                "Receiver stats: {} packets, {} bytes, {} invalid",
                recv_stats.packets_received,
                recv_stats.bytes_received,
                recv_stats.invalid_packets
            );
            
            let states = track_states.lock();
            for (track_id, state) in states.iter() {
                let jitter_stats = state.jitter_buffer.stats();
                tracing::info!(
                    "Track {} stats: {} received, {} lost ({:.1}% loss), jitter buffer: {}/{}",
                    track_id,
                    state.packets_received,
                    state.packets_lost,
                    jitter_stats.loss_rate() * 100.0,
                    jitter_stats.level,
                    jitter_stats.capacity
                );
            }
        }
        
        // Small sleep to prevent busy-waiting
        tokio::time::sleep(Duration::from_micros(500)).await;
    }
}
