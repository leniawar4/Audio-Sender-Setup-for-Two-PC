//! Audio Sender Application
//!
//! Captures audio from multiple devices and streams to receiver over UDP.

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use lan_audio_streamer::{
    audio::{
        buffer::{create_shared_buffer, SharedRingBuffer},
        capture::AudioCapture,
        device::list_devices,
    },
    codec::OpusEncoder,
    config::{AppConfig, OpusConfig},
    constants::*,
    network::{
        sender::MultiTrackSender,
        discovery::{DiscoveryService, get_best_local_address, get_local_addresses},
    },
    protocol::{TrackConfig, TrackType},
    tracks::{TrackManager, TrackEvent},
    ui::WebServer,
};

/// Per-track sender state including capture and encoder
struct TrackSenderState {
    capture: AudioCapture,
    capture_buffer: SharedRingBuffer,
    encoder: OpusEncoder,
    sample_buffer: Vec<f32>,
    sequence: u32,
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
    
    tracing::info!("Starting LAN Audio Sender");
    
    // Load or create config
    let config = AppConfig::default();
    
    // List available devices
    println!("\n=== Available Audio Devices ===");
    let devices = list_devices();
    for device in &devices {
        let device_type = match (device.is_input, device.is_output) {
            (true, true) => "Input/Output",
            (true, false) => "Input",
            (false, true) => "Output",
            _ => "Unknown",
        };
        let default_marker = if device.is_default { " [DEFAULT]" } else { "" };
        println!("  {} ({}){}:", device.name, device_type, default_marker);
        println!("    ID: {}", device.id);
        println!("    Sample rates: {:?}", device.sample_rates);
        println!("    Channels: {:?}", device.channels);
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
        true, // is_sender
    );
    let _web_handle = web_server.start_background();
    
    tracing::info!("Web UI available at http://{}:{}", config.ui.bind_address, config.ui.http_port);
    
    // Display local network addresses for user reference
    println!("\n=== Local Network Addresses ===");
    for addr in get_local_addresses() {
        println!("  {}", addr);
    }
    if let Some(best) = get_best_local_address() {
        println!("  Best for LAN: {}", best);
    }
    println!();
    
    // Get target address - automatic discovery or manual
    let target_addr: SocketAddr = if let Some(arg) = std::env::args().nth(1) {
        // Manual address provided
        arg.parse().expect("Invalid target address format. Use: IP:PORT")
    } else {
        // Automatic discovery
        tracing::info!("No target specified, starting automatic receiver discovery...");
        println!("Searching for receivers on the network...");
        
        let mut discovery = DiscoveryService::new(true, config.network.udp_port, "Audio Sender".to_string());
        if let Err(e) = discovery.start() {
            tracing::warn!("Failed to start discovery service: {}", e);
        }
        
        // Wait for a receiver with timeout
        let receiver = discovery.wait_for_peer(false, Duration::from_secs(30));
        
        if let Some(peer) = receiver {
            let addr = peer.audio_address();
            tracing::info!("Discovered receiver: {} ({})", peer.name, addr);
            println!("Found receiver: {} at {}", peer.name, addr);
            discovery.stop();
            addr
        } else {
            // Fallback: broadcast to default port
            let best_local = get_best_local_address()
                .map(|ip| match ip {
                    std::net::IpAddr::V4(v4) => {
                        let octets = v4.octets();
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], 255))
                    }
                    ip => ip,
                })
                .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::BROADCAST));
            
            let addr = SocketAddr::new(best_local, DEFAULT_UDP_PORT);
            tracing::warn!("No receiver found, broadcasting to {}", addr);
            println!("No receiver found. Broadcasting to {}", addr);
            discovery.stop();
            addr
        }
    };
    
    tracing::info!("Target receiver: {}", target_addr);
    
    // Create network sender
    let mut network_sender = MultiTrackSender::new(&config.network, target_addr)?;
    network_sender.start(config.network.clone())?;
    
    tracing::info!("Network sender started");
    
    // Track states - shared mutable map for runtime reconfiguration
    let track_states: Arc<Mutex<HashMap<u8, TrackSenderState>>> = Arc::new(Mutex::new(HashMap::new()));
    let track_states_for_events = track_states.clone();
    let track_manager_for_events = track_manager.clone();
    
    // Spawn task to handle track events (device changes, track creation/removal)
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    match event {
                        TrackEvent::Created(track_id) => {
                            tracing::info!("Track {} created, initializing capture...", track_id);
                            
                            // Get track config
                            if let Some(track) = track_manager_for_events.get_track(track_id) {
                                let device_id = track.device_id.clone();
                                drop(track); // Release lock
                                
                                if let Err(e) = create_capture_for_track(
                                    track_id,
                                    &device_id,
                                    &track_states_for_events
                                ) {
                                    tracing::error!("Failed to create capture for track {}: {}", track_id, e);
                                }
                            }
                        }
                        
                        TrackEvent::Removed(track_id) => {
                            tracing::info!("Track {} removed, stopping capture...", track_id);
                            let mut states = track_states_for_events.lock();
                            if let Some(mut state) = states.remove(&track_id) {
                                state.capture.stop();
                                tracing::info!("Capture stopped for track {}", track_id);
                            }
                        }
                        
                        TrackEvent::DeviceChanged(track_id, old_device, new_device) => {
                            tracing::info!(
                                "Track {} device changed: {} -> {}",
                                track_id, old_device, new_device
                            );
                            
                            // Stop old capture
                            {
                                let mut states = track_states_for_events.lock();
                                if let Some(mut state) = states.remove(&track_id) {
                                    state.capture.stop();
                                    tracing::info!("Stopped old capture for track {}", track_id);
                                }
                            }
                            
                            // Create new capture with new device
                            if let Err(e) = create_capture_for_track(
                                track_id,
                                &new_device,
                                &track_states_for_events
                            ) {
                                tracing::error!(
                                    "Failed to create capture for track {} on device {}: {}",
                                    track_id, new_device, e
                                );
                            } else {
                                tracing::info!(
                                    "Successfully switched track {} to device {}",
                                    track_id, new_device
                                );
                            }
                        }
                        
                        _ => {
                            // Other events (Started, Stopped, ConfigUpdated) - handle as needed
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Event channel error: {}", e);
                    // Channel lagged, continue
                }
            }
        }
    });
    
    // Create initial track from default input device (if available)
    if let Some(input_device) = devices.iter().find(|d| d.is_input && d.is_default) {
        let track_config = TrackConfig {
            track_id: Some(0),
            name: format!("Default Input - {}", input_device.name),
            device_id: input_device.id.clone(),
            bitrate: 128_000,
            frame_size_ms: 10.0,
            channels: 2,
            track_type: TrackType::Music,
            fec_enabled: false,
        };
        
        let _track_id = track_manager.create_track(track_config)?;
        tracing::info!("Created initial track for device {}", input_device.name);
        
        // Note: The event handler will create the capture automatically
    }
    
    let start_time = Instant::now();
    let mut last_stats_time = Instant::now();
    
    tracing::info!("Starting main loop - press Ctrl+C to stop");
    
    // Main encoding/sending loop
    loop {
        // Process all tracks with minimal blocking
        let has_work = {
            let mut states = track_states.lock();
            let mut work_done = false;
            
            for (track_id, state) in states.iter_mut() {
                let frame_size = state.encoder.samples_per_frame();
                
                // Drain all available captured audio
                while let Some(frame) = state.capture_buffer.try_pop() {
                    work_done = true;
                    
                    // Accumulate samples
                    state.sample_buffer.extend_from_slice(&frame.samples);
                    
                    // Update audio level for the track
                    if let Some(track) = track_manager.get_track(*track_id) {
                        track.update_level_atomic(&frame.samples);
                    }
                    
                    // Process complete frames immediately
                    while state.sample_buffer.len() >= frame_size {
                        let samples: Vec<f32> = state.sample_buffer.drain(..frame_size).collect();
                        
                        // Encode
                        match state.encoder.encode(&samples) {
                            Ok(encoded) => {
                                // Calculate timestamp from start
                                let timestamp = start_time.elapsed().as_micros() as u64;
                                
                                // Send over network immediately
                                if let Err(e) = network_sender.send_audio(
                                    *track_id,
                                    encoded,
                                    timestamp,
                                    DEFAULT_CHANNELS == 2,
                                ) {
                                    // Only log occasionally to prevent spam
                                    if state.sequence % 1000 == 0 {
                                        tracing::warn!("Failed to send packet for track {}: {}", track_id, e);
                                    }
                                } else {
                                    // Update packet count on successful send
                                    if let Some(track) = track_manager.get_track(*track_id) {
                                        track.increment_packets();
                                        
                                        // Calculate latency estimate from encode time
                                        let encode_time_us = (state.encoder.frame_duration_ms() * 1000.0) as u32;
                                        track.update_latency(encode_time_us);
                                    }
                                }
                                
                                state.sequence = state.sequence.wrapping_add(1);
                            }
                            Err(e) => {
                                tracing::warn!("Encoding failed for track {}: {}", track_id, e);
                            }
                        }
                    }
                }
            }
            work_done
        };
        
        // Adaptive sleep: shorter when active, longer when idle
        if has_work {
            // Yield briefly to allow other tasks, but stay responsive
            tokio::task::yield_now().await;
        } else {
            // No work - wait with timeout for efficiency
            tokio::time::sleep(Duration::from_micros(250)).await;
        }
        
        // Periodic stats logging
        if last_stats_time.elapsed() >= Duration::from_secs(5) {
            last_stats_time = Instant::now();
            
            let sender_stats = network_sender.stats();
            let states = track_states.lock();
            tracing::info!(
                "Sender stats: {} tracks active, {} packets sent, {:.1} KB sent",
                states.len(),
                sender_stats.packets_sent,
                sender_stats.bytes_sent as f64 / 1024.0,
            );
        }
    }
}

/// Create a new capture instance for a track
fn create_capture_for_track(
    track_id: u8,
    device_id: &str,
    track_states: &Arc<Mutex<HashMap<u8, TrackSenderState>>>,
) -> Result<()> {
    // Create capture buffer
    let capture_buffer = create_shared_buffer(RING_BUFFER_CAPACITY);
    
    // Create and start audio capture
    let mut capture = AudioCapture::new(
        track_id,
        device_id,
        Some(DEFAULT_SAMPLE_RATE),
        Some(DEFAULT_CHANNELS),
        None,
        capture_buffer.clone(),
    )?;
    
    capture.start()?;
    tracing::info!("Audio capture started for track {} on device {}", track_id, device_id);
    
    // Create Opus encoder for this track
    let opus_config = OpusConfig::music();
    let encoder = OpusEncoder::new(opus_config)?;
    let frame_size = encoder.samples_per_frame();
    
    tracing::info!(
        "Opus encoder initialized for track {}: {}Hz, {} channels, {} samples/frame ({:.1}ms)",
        track_id,
        DEFAULT_SAMPLE_RATE,
        DEFAULT_CHANNELS,
        frame_size,
        encoder.frame_duration_ms()
    );
    
    // Store state
    let state = TrackSenderState {
        capture,
        capture_buffer,
        encoder,
        sample_buffer: Vec::with_capacity(frame_size * 2),
        sequence: 0,
    };
    
    let mut states = track_states.lock();
    states.insert(track_id, state);
    
    Ok(())
}
