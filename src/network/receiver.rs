//! Audio packet receiver
//!
//! Handles receiving audio packets and demultiplexing by track ID.

use bytes::Bytes;
use crossbeam_channel::Sender;
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::error::NetworkError;
use crate::network::udp::create_socket;
use crate::protocol::AudioPacket;
use crate::config::NetworkConfig;

/// Received packet ready for decoding
#[derive(Debug, Clone)]
pub struct ReceivedPacket {
    pub track_id: u8,
    pub sequence: u32,
    pub timestamp: u64,
    pub payload: Bytes,
    pub is_stereo: bool,
    pub has_fec: bool,
    pub receive_time: std::time::Instant,
}

impl From<AudioPacket> for ReceivedPacket {
    fn from(packet: AudioPacket) -> Self {
        Self {
            track_id: packet.track_id,
            sequence: packet.sequence,
            timestamp: packet.timestamp,
            payload: packet.payload,
            is_stereo: packet.flags.is_stereo(),
            has_fec: packet.flags.has_fec(),
            receive_time: std::time::Instant::now(),
        }
    }
}

/// Callback type for received packets
pub type PacketCallback = Box<dyn Fn(ReceivedPacket) + Send + Sync>;

/// Audio receiver for multiple tracks
pub struct AudioReceiver {
    /// Receiver thread handle
    thread_handle: Option<JoinHandle<()>>,
    
    /// Running flag
    running: Arc<AtomicBool>,
    
    /// Packets received counter
    packets_received: Arc<AtomicU64>,
    
    /// Bytes received counter
    bytes_received: Arc<AtomicU64>,
    
    /// Invalid packets counter
    invalid_packets: Arc<AtomicU64>,
    
    /// Per-track packet channels
    track_channels: Arc<DashMap<u8, Sender<ReceivedPacket>>>,
    
    /// Global packet channel (for all tracks)
    global_tx: Option<Sender<ReceivedPacket>>,
}

impl AudioReceiver {
    /// Create a new audio receiver
    pub fn new() -> Self {
        Self {
            thread_handle: None,
            running: Arc::new(AtomicBool::new(false)),
            packets_received: Arc::new(AtomicU64::new(0)),
            bytes_received: Arc::new(AtomicU64::new(0)),
            invalid_packets: Arc::new(AtomicU64::new(0)),
            track_channels: Arc::new(DashMap::new()),
            global_tx: None,
        }
    }
    
    /// Set global packet channel
    pub fn set_global_channel(&mut self, tx: Sender<ReceivedPacket>) {
        self.global_tx = Some(tx);
    }
    
    /// Register a channel for a specific track
    pub fn register_track(&self, track_id: u8, tx: Sender<ReceivedPacket>) {
        self.track_channels.insert(track_id, tx);
    }
    
    /// Unregister a track channel
    pub fn unregister_track(&self, track_id: u8) {
        self.track_channels.remove(&track_id);
    }
    
    /// Start the receiver thread
    pub fn start(&mut self, config: NetworkConfig) -> Result<(), NetworkError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        let socket = create_socket(&config)?;
        
        let running = self.running.clone();
        let packets_received = self.packets_received.clone();
        let bytes_received = self.bytes_received.clone();
        let invalid_packets = self.invalid_packets.clone();
        let track_channels = self.track_channels.clone();
        let global_tx = self.global_tx.clone();
        
        running.store(true, Ordering::SeqCst);
        
        let handle = thread::Builder::new()
            .name("audio-receiver".to_string())
            .spawn(move || {
                // Use larger buffer to handle MTU + headers
                let mut recv_buffer = vec![0u8; 2048];
                
                // Adaptive backoff for empty reads
                let mut empty_reads = 0u32;
                const MAX_EMPTY_READS: u32 = 100;
                
                while running.load(Ordering::Relaxed) {
                    match socket.recv_from(&mut recv_buffer) {
                        Ok((size, _addr)) => {
                            // Reset empty read counter on successful receive
                            empty_reads = 0;
                            
                            bytes_received.fetch_add(size as u64, Ordering::Relaxed);
                            
                            // Parse packet
                            let data = Bytes::copy_from_slice(&recv_buffer[..size]);
                            if let Some(packet) = AudioPacket::deserialize(data) {
                                packets_received.fetch_add(1, Ordering::Relaxed);
                                
                                let received = ReceivedPacket::from(packet);
                                let track_id = received.track_id;
                                
                                // Send to track-specific channel (non-blocking)
                                if let Some(tx) = track_channels.get(&track_id) {
                                    let _ = tx.try_send(received.clone());
                                }
                                
                                // Send to global channel (non-blocking)
                                if let Some(ref tx) = global_tx {
                                    let _ = tx.try_send(received);
                                }
                            } else {
                                invalid_packets.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // Adaptive backoff: start with spin, then yield, then sleep
                            empty_reads = empty_reads.saturating_add(1);
                            
                            if empty_reads < 10 {
                                // Spin - lowest latency for bursty traffic
                                std::hint::spin_loop();
                            } else if empty_reads < MAX_EMPTY_READS {
                                // Yield to other threads
                                thread::yield_now();
                            } else {
                                // Brief sleep to prevent CPU hogging during silence
                                thread::sleep(std::time::Duration::from_micros(50));
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::Interrupted {
                                tracing::warn!("Receive error: {}", e);
                            }
                            thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
            })
            .map_err(|e| NetworkError::ReceiveFailed(e.to_string()))?;
        
        self.thread_handle = Some(handle);
        Ok(())
    }
    
    /// Stop the receiver
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
    
    /// Check if running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    
    /// Get packets received count
    pub fn packets_received(&self) -> u64 {
        self.packets_received.load(Ordering::Relaxed)
    }
    
    /// Get bytes received count
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received.load(Ordering::Relaxed)
    }
    
    /// Get invalid packets count
    pub fn invalid_packets(&self) -> u64 {
        self.invalid_packets.load(Ordering::Relaxed)
    }
    
    /// Get statistics
    pub fn stats(&self) -> ReceiverStats {
        ReceiverStats {
            packets_received: self.packets_received(),
            bytes_received: self.bytes_received(),
            invalid_packets: self.invalid_packets(),
            registered_tracks: self.track_channels.len(),
        }
    }
}

impl Default for AudioReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Receiver statistics
#[derive(Debug, Clone)]
pub struct ReceiverStats {
    pub packets_received: u64,
    pub bytes_received: u64,
    pub invalid_packets: u64,
    pub registered_tracks: usize,
}

/// Per-track receiver that processes packets for a single track
pub struct TrackReceiver {
    track_id: u8,
    packet_rx: crossbeam_channel::Receiver<ReceivedPacket>,
    last_sequence: Option<u32>,
    packets_received: u64,
    packets_lost: u64,
    out_of_order: u64,
}

impl TrackReceiver {
    pub fn new(track_id: u8, packet_rx: crossbeam_channel::Receiver<ReceivedPacket>) -> Self {
        Self {
            track_id,
            packet_rx,
            last_sequence: None,
            packets_received: 0,
            packets_lost: 0,
            out_of_order: 0,
        }
    }
    
    /// Receive next packet (blocking)
    pub fn recv(&mut self) -> Result<ReceivedPacket, crossbeam_channel::RecvError> {
        let packet = self.packet_rx.recv()?;
        self.process_sequence(packet.sequence);
        self.packets_received += 1;
        Ok(packet)
    }
    
    /// Try to receive packet (non-blocking)
    pub fn try_recv(&mut self) -> Option<ReceivedPacket> {
        match self.packet_rx.try_recv() {
            Ok(packet) => {
                self.process_sequence(packet.sequence);
                self.packets_received += 1;
                Some(packet)
            }
            Err(_) => None,
        }
    }
    
    /// Receive with timeout
    pub fn recv_timeout(&mut self, timeout: std::time::Duration) -> Option<ReceivedPacket> {
        match self.packet_rx.recv_timeout(timeout) {
            Ok(packet) => {
                self.process_sequence(packet.sequence);
                self.packets_received += 1;
                Some(packet)
            }
            Err(_) => None,
        }
    }
    
    /// Process sequence number for statistics
    fn process_sequence(&mut self, sequence: u32) {
        if let Some(last) = self.last_sequence {
            let expected = last.wrapping_add(1);
            if sequence != expected {
                if sequence > expected {
                    // Packets lost
                    let lost = sequence.wrapping_sub(expected);
                    self.packets_lost += lost as u64;
                } else {
                    // Out of order
                    self.out_of_order += 1;
                }
            }
        }
        self.last_sequence = Some(sequence);
    }
    
    /// Get track ID
    pub fn track_id(&self) -> u8 {
        self.track_id
    }
    
    /// Get statistics
    pub fn stats(&self) -> TrackReceiverStats {
        TrackReceiverStats {
            track_id: self.track_id,
            packets_received: self.packets_received,
            packets_lost: self.packets_lost,
            out_of_order: self.out_of_order,
            loss_rate: if self.packets_received + self.packets_lost > 0 {
                self.packets_lost as f32 / (self.packets_received + self.packets_lost) as f32
            } else {
                0.0
            },
        }
    }
}

/// Track receiver statistics
#[derive(Debug, Clone)]
pub struct TrackReceiverStats {
    pub track_id: u8,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub out_of_order: u64,
    pub loss_rate: f32,
}
