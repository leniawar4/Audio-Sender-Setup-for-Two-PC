//! Audio packet sender
//!
//! Handles sending encoded audio packets over UDP with proper
//! sequencing and timing.

use bytes::Bytes;
use crossbeam_channel::Receiver;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::error::NetworkError;
use crate::network::udp::{create_socket, PacketSender};
use crate::protocol::{AudioPacket, PacketFlags};
use crate::config::NetworkConfig;

/// Encoded packet ready for sending
pub struct EncodedPacket {
    pub track_id: u8,
    pub sequence: u32,
    pub timestamp: u64,
    pub payload: Bytes,
    pub flags: PacketFlags,
}

/// Audio sender for multiple tracks
pub struct AudioSender {
    /// Sender thread handle
    thread_handle: Option<JoinHandle<()>>,
    
    /// Running flag
    running: Arc<AtomicBool>,
    
    /// Packets sent counter
    packets_sent: Arc<AtomicU64>,
    
    /// Bytes sent counter
    bytes_sent: Arc<AtomicU64>,
    
    /// Input channel for packets
    packet_tx: crossbeam_channel::Sender<EncodedPacket>,
    
    /// Target address
    target_addr: SocketAddr,
}

impl AudioSender {
    /// Create a new audio sender
    pub fn new(
        config: &NetworkConfig,
        target_addr: SocketAddr,
    ) -> Result<Self, NetworkError> {
        let _socket = create_socket(config)?;
        
        let (packet_tx, _packet_rx) = crossbeam_channel::bounded::<EncodedPacket>(1024);
        
        let running = Arc::new(AtomicBool::new(false));
        let packets_sent = Arc::new(AtomicU64::new(0));
        let bytes_sent = Arc::new(AtomicU64::new(0));
        
        Ok(Self {
            thread_handle: None,
            running,
            packets_sent,
            bytes_sent,
            packet_tx,
            target_addr,
        })
    }
    
    /// Start the sender thread
    pub fn start(&mut self, config: NetworkConfig) -> Result<(), NetworkError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        let socket = create_socket(&config)?;
        let sender = PacketSender::new(socket, self.target_addr);
        
        let (packet_tx, packet_rx) = crossbeam_channel::bounded::<EncodedPacket>(1024);
        self.packet_tx = packet_tx;
        
        let running = self.running.clone();
        let packets_sent = self.packets_sent.clone();
        let bytes_sent = self.bytes_sent.clone();
        
        running.store(true, Ordering::SeqCst);
        
        let handle = thread::Builder::new()
            .name("audio-sender".to_string())
            .spawn(move || {
                Self::sender_loop(sender, packet_rx, running, packets_sent, bytes_sent);
            })
            .map_err(|e| NetworkError::SendFailed(e.to_string()))?;
        
        self.thread_handle = Some(handle);
        Ok(())
    }
    
    /// Sender loop
    fn sender_loop(
        sender: PacketSender,
        packet_rx: Receiver<EncodedPacket>,
        running: Arc<AtomicBool>,
        packets_sent: Arc<AtomicU64>,
        bytes_sent: Arc<AtomicU64>,
    ) {
        // Adaptive timeout: start fast, slow down during silence
        let mut consecutive_timeouts = 0u32;
        const MAX_CONSECUTIVE_TIMEOUTS: u32 = 100;
        
        while running.load(Ordering::Relaxed) {
            // Adaptive timeout based on traffic pattern
            let timeout = if consecutive_timeouts < 10 {
                std::time::Duration::from_micros(100) // Fast polling during active streaming
            } else if consecutive_timeouts < MAX_CONSECUTIVE_TIMEOUTS {
                std::time::Duration::from_millis(1) // Medium polling
            } else {
                std::time::Duration::from_millis(5) // Slow polling during silence
            };
            
            match packet_rx.recv_timeout(timeout) {
                Ok(encoded) => {
                    consecutive_timeouts = 0; // Reset on successful receive
                    
                    // Create audio packet
                    let packet = AudioPacket {
                        track_id: encoded.track_id,
                        flags: encoded.flags,
                        sequence: encoded.sequence,
                        timestamp: encoded.timestamp,
                        payload: encoded.payload,
                    };
                    
                    // Serialize and send
                    let data = packet.serialize();
                    match sender.send(&data) {
                        Ok(sent) => {
                            packets_sent.fetch_add(1, Ordering::Relaxed);
                            bytes_sent.fetch_add(sent as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            // Only log periodically to avoid log spam
                            if packets_sent.load(Ordering::Relaxed) % 1000 == 0 {
                                tracing::warn!("Failed to send packet: {}", e);
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    consecutive_timeouts = consecutive_timeouts.saturating_add(1);
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    // Channel closed, exit
                    break;
                }
            }
        }
    }
    
    /// Stop the sender
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
    
    /// Send an encoded packet
    pub fn send(&self, packet: EncodedPacket) -> Result<(), NetworkError> {
        self.packet_tx
            .try_send(packet)
            .map_err(|_| NetworkError::SendFailed("Channel full".to_string()))
    }
    
    /// Get channel for sending packets
    pub fn sender(&self) -> crossbeam_channel::Sender<EncodedPacket> {
        self.packet_tx.clone()
    }
    
    /// Check if running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    
    /// Get packets sent count
    pub fn packets_sent(&self) -> u64 {
        self.packets_sent.load(Ordering::Relaxed)
    }
    
    /// Get bytes sent count  
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }
    
    /// Update target address
    pub fn set_target(&mut self, addr: SocketAddr) {
        self.target_addr = addr;
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Multi-track sender that aggregates packets from multiple tracks
pub struct MultiTrackSender {
    inner: AudioSender,
    /// Per-track sequence counters
    sequences: dashmap::DashMap<u8, u32>,
}

impl MultiTrackSender {
    pub fn new(config: &NetworkConfig, target_addr: SocketAddr) -> Result<Self, NetworkError> {
        Ok(Self {
            inner: AudioSender::new(config, target_addr)?,
            sequences: dashmap::DashMap::new(),
        })
    }
    
    /// Start sender
    pub fn start(&mut self, config: NetworkConfig) -> Result<(), NetworkError> {
        self.inner.start(config)
    }
    
    /// Stop sender
    pub fn stop(&mut self) {
        self.inner.stop();
    }
    
    /// Send encoded audio for a track
    pub fn send_audio(
        &self,
        track_id: u8,
        payload: Bytes,
        timestamp: u64,
        stereo: bool,
    ) -> Result<u32, NetworkError> {
        // Get and increment sequence
        let sequence = {
            let mut entry = self.sequences.entry(track_id).or_insert(0);
            let seq = *entry;
            *entry = entry.wrapping_add(1);
            seq
        };
        
        let packet = EncodedPacket {
            track_id,
            sequence,
            timestamp,
            payload,
            flags: PacketFlags::new().set_stereo(stereo),
        };
        
        self.inner.send(packet)?;
        Ok(sequence)
    }
    
    /// Reset sequence counter for a track
    pub fn reset_sequence(&self, track_id: u8) {
        self.sequences.insert(track_id, 0);
    }
    
    /// Remove track
    pub fn remove_track(&self, track_id: u8) {
        self.sequences.remove(&track_id);
    }
    
    /// Get sender channel
    pub fn sender(&self) -> crossbeam_channel::Sender<EncodedPacket> {
        self.inner.sender()
    }
    
    /// Get statistics
    pub fn stats(&self) -> SenderStats {
        SenderStats {
            packets_sent: self.inner.packets_sent(),
            bytes_sent: self.inner.bytes_sent(),
            active_tracks: self.sequences.len(),
        }
    }
}

/// Sender statistics
#[derive(Debug, Clone)]
pub struct SenderStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub active_tracks: usize,
}
