//! Protocol definitions for audio streaming packets
//!
//! ## Packet Format
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────────────┐
//! │                        Audio Packet Header (16 bytes)                  │
//! ├──────────┬──────────┬──────────┬──────────┬────────────────────────────┤
//! │ Magic(2) │TrackID(1)│ Flags(1) │  Seq(4)  │      Timestamp(8)          │
//! │  0xAF01  │   0-255  │ See below│ u32 LE   │      u64 LE (µs)           │
//! ├──────────┴──────────┴──────────┴──────────┴────────────────────────────┤
//! │                        Opus Payload (variable)                         │
//! │                        Max: 1456 bytes                                 │
//! └────────────────────────────────────────────────────────────────────────┘
//!
//! Flags byte:
//! ┌─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┐
//! │  7  │  6  │  5  │  4  │  3  │  2  │  1  │  0  │
//! │ RSV │ RSV │ RSV │ RSV │ RSV │ FEC │STEREO│KEYF│
//! └─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┘
//! ```

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

/// Magic number for packet identification
pub const PACKET_MAGIC: u16 = 0xAF01;

/// Maximum payload size (MTU - IP header - UDP header - our header)
pub const MAX_PAYLOAD_SIZE: usize = 1456;

/// Header size in bytes
pub const HEADER_SIZE: usize = 16;

/// Packet flags
#[derive(Debug, Clone, Copy, Default)]
pub struct PacketFlags(u8);

impl PacketFlags {
    pub const KEYFRAME: u8 = 0x01;
    pub const STEREO: u8 = 0x02;
    pub const FEC: u8 = 0x04;
    
    pub fn new() -> Self {
        Self(0)
    }
    
    pub fn set_keyframe(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::KEYFRAME;
        } else {
            self.0 &= !Self::KEYFRAME;
        }
        self
    }
    
    pub fn set_stereo(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::STEREO;
        } else {
            self.0 &= !Self::STEREO;
        }
        self
    }
    
    pub fn set_fec(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::FEC;
        } else {
            self.0 &= !Self::FEC;
        }
        self
    }
    
    pub fn is_keyframe(&self) -> bool {
        self.0 & Self::KEYFRAME != 0
    }
    
    pub fn is_stereo(&self) -> bool {
        self.0 & Self::STEREO != 0
    }
    
    pub fn has_fec(&self) -> bool {
        self.0 & Self::FEC != 0
    }
    
    pub fn as_byte(&self) -> u8 {
        self.0
    }
    
    pub fn from_byte(byte: u8) -> Self {
        Self(byte)
    }
}

/// Audio packet for network transmission
#[derive(Debug, Clone)]
pub struct AudioPacket {
    /// Track identifier (0-255)
    pub track_id: u8,
    
    /// Packet flags
    pub flags: PacketFlags,
    
    /// Sequence number for reordering
    pub sequence: u32,
    
    /// Capture timestamp in microseconds
    pub timestamp: u64,
    
    /// Opus-encoded audio data
    pub payload: Bytes,
}

impl AudioPacket {
    /// Create a new audio packet
    pub fn new(track_id: u8, sequence: u32, timestamp: u64, payload: Bytes) -> Self {
        Self {
            track_id,
            flags: PacketFlags::new(),
            sequence,
            timestamp,
            payload,
        }
    }
    
    /// Serialize packet to bytes for network transmission
    pub fn serialize(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        
        // Magic number
        buf.put_u16_le(PACKET_MAGIC);
        // Track ID
        buf.put_u8(self.track_id);
        // Flags
        buf.put_u8(self.flags.as_byte());
        // Sequence number
        buf.put_u32_le(self.sequence);
        // Timestamp
        buf.put_u64_le(self.timestamp);
        // Payload
        buf.put_slice(&self.payload);
        
        buf.freeze()
    }
    
    /// Deserialize packet from bytes
    pub fn deserialize(mut data: Bytes) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        
        // Check magic number
        let magic = data.get_u16_le();
        if magic != PACKET_MAGIC {
            return None;
        }
        
        let track_id = data.get_u8();
        let flags = PacketFlags::from_byte(data.get_u8());
        let sequence = data.get_u32_le();
        let timestamp = data.get_u64_le();
        let payload = data; // Remaining bytes are payload
        
        Some(Self {
            track_id,
            flags,
            sequence,
            timestamp,
            payload,
        })
    }
    
    /// Get packet size including header
    pub fn total_size(&self) -> usize {
        HEADER_SIZE + self.payload.len()
    }
}

/// Response for device list with receiver/sender flag
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicesResponse {
    pub devices: Vec<AudioDeviceInfo>,
    pub is_receiver: bool,
}

/// Control message types for WebSocket communication
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ControlMessage {
    /// Create a new track
    CreateTrack(TrackConfig),
    
    /// Remove an existing track
    RemoveTrack { track_id: u8 },
    
    /// Update track configuration
    UpdateTrack { track_id: u8, config: TrackConfigUpdate },
    
    /// Mute/unmute a track
    SetMute { track_id: u8, muted: bool },
    
    /// Solo a track
    SetSolo { track_id: u8, solo: bool },
    
    /// Get track status
    GetStatus,
    
    /// Status response
    Status(Vec<TrackStatus>),
    
    /// List available audio devices
    ListDevices,
    
    /// Device list response
    Devices(DevicesResponse),
    
    /// Error response
    Error { message: String },
    
    /// Ping for keepalive
    Ping,
    
    /// Pong response
    Pong,
}

/// Track configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackConfig {
    /// Track ID (optional, auto-assigned if not provided)
    pub track_id: Option<u8>,
    
    /// Human-readable track name
    pub name: String,
    
    /// Audio device identifier
    pub device_id: String,
    
    /// Target bitrate in bits per second
    pub bitrate: u32,
    
    /// Frame size in milliseconds (2.5, 5, 10, 20)
    pub frame_size_ms: f32,
    
    /// Number of channels (1 or 2)
    pub channels: u16,
    
    /// Track type (affects Opus tuning)
    pub track_type: TrackType,
    
    /// Enable FEC (Forward Error Correction)
    pub fec_enabled: bool,
}

impl Default for TrackConfig {
    fn default() -> Self {
        Self {
            track_id: None,
            name: String::from("New Track"),
            device_id: String::new(),
            bitrate: 128_000,
            frame_size_ms: 10.0,
            channels: 2,
            track_type: TrackType::Music,
            fec_enabled: false,
        }
    }
}

/// Partial track configuration for updates
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackConfigUpdate {
    pub name: Option<String>,
    pub device_id: Option<String>,
    pub bitrate: Option<u32>,
    pub frame_size_ms: Option<f32>,
    pub fec_enabled: Option<bool>,
}

/// Track type for Opus optimization
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TrackType {
    /// Voice/speech - optimized for intelligibility
    Voice,
    /// Music - optimized for audio quality
    Music,
    /// Low latency - minimal algorithmic delay
    LowLatency,
}

impl Default for TrackType {
    fn default() -> Self {
        Self::Music
    }
}

/// Информация о статусе трека
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackStatus {
    pub track_id: u8,
    pub name: String,
    pub device_id: String,
    pub active: bool,
    pub muted: bool,
    pub solo: bool,
    pub bitrate: u32,
    pub frame_size_ms: f32,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub current_latency_ms: f32,
    pub jitter_ms: f32,
    /// Текущий сглаженный уровень в dB
    pub level_db: f32,
    /// Пиковый уровень в dB (с удержанием)
    pub peak_db: f32,
    /// Нормализованный уровень (0.0 - 1.0) для UI
    pub level_normalized: f32,
    /// Нормализованный пик (0.0 - 1.0) для UI
    pub peak_normalized: f32,
}

/// Audio device information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_input: bool,
    pub is_output: bool,
    pub is_default: bool,
    pub sample_rates: Vec<u32>,
    pub channels: Vec<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_packet_serialization() {
        let packet = AudioPacket {
            track_id: 5,
            flags: PacketFlags::new().set_stereo(true).set_keyframe(true),
            sequence: 12345,
            timestamp: 9876543210,
            payload: Bytes::from_static(&[1, 2, 3, 4, 5]),
        };
        
        let serialized = packet.serialize();
        let deserialized = AudioPacket::deserialize(serialized).unwrap();
        
        assert_eq!(deserialized.track_id, 5);
        assert!(deserialized.flags.is_stereo());
        assert!(deserialized.flags.is_keyframe());
        assert_eq!(deserialized.sequence, 12345);
        assert_eq!(deserialized.timestamp, 9876543210);
        assert_eq!(deserialized.payload.as_ref(), &[1, 2, 3, 4, 5]);
    }
    
    #[test]
    fn test_flags() {
        let flags = PacketFlags::new()
            .set_keyframe(true)
            .set_stereo(true)
            .set_fec(true);
        
        assert!(flags.is_keyframe());
        assert!(flags.is_stereo());
        assert!(flags.has_fec());
        assert_eq!(flags.as_byte(), 0x07);
    }
}
