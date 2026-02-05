//! Configuration management

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use crate::constants::*;
use crate::protocol::{TrackConfig, TrackType};

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Network configuration
    pub network: NetworkConfig,
    
    /// Audio configuration
    pub audio: AudioConfig,
    
    /// UI configuration
    pub ui: UiConfig,
    
    /// Pre-configured tracks
    pub tracks: Vec<TrackConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig::default(),
            audio: AudioConfig::default(),
            ui: UiConfig::default(),
            tracks: Vec::new(),
        }
    }
}

/// Network configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Local bind address
    pub bind_address: String,
    
    /// UDP port for audio streaming
    pub udp_port: u16,
    
    /// Remote destination address (for sender)
    pub remote_address: Option<String>,
    
    /// Socket send buffer size
    pub send_buffer_size: usize,
    
    /// Socket receive buffer size
    pub recv_buffer_size: usize,
    
    /// Enable SO_REUSEADDR
    pub reuse_addr: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            udp_port: DEFAULT_UDP_PORT,
            remote_address: None,
            send_buffer_size: 4 * 1024 * 1024, // 4 MB - larger to handle bursts
            recv_buffer_size: 4 * 1024 * 1024, // 4 MB - larger to prevent drops
            reuse_addr: true,
        }
    }
}

/// Audio configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Default sample rate
    pub sample_rate: u32,
    
    /// Default channel count
    pub channels: u16,
    
    /// Default bitrate
    pub default_bitrate: u32,
    
    /// Default frame size in ms
    pub default_frame_size_ms: f32,
    
    /// Default jitter buffer size in ms
    pub jitter_buffer_ms: u32,
    
    /// Enable WASAPI exclusive mode (Windows)
    pub wasapi_exclusive: bool,
    
    /// Use low-latency WASAPI shared mode
    pub wasapi_low_latency: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            default_bitrate: DEFAULT_BITRATE,
            default_frame_size_ms: DEFAULT_FRAME_SIZE_MS,
            jitter_buffer_ms: DEFAULT_JITTER_BUFFER_MS,
            wasapi_exclusive: false,
            wasapi_low_latency: true,
        }
    }
}

/// UI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// HTTP server port
    pub http_port: u16,
    
    /// WebSocket port (usually same as HTTP)
    pub ws_port: u16,
    
    /// Bind address for web server
    pub bind_address: String,
    
    /// Enable CORS
    pub enable_cors: bool,
    
    /// Static files directory
    pub static_dir: Option<PathBuf>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            http_port: DEFAULT_WS_PORT,
            ws_port: DEFAULT_WS_PORT,
            bind_address: "127.0.0.1".to_string(),
            enable_cors: true,
            static_dir: None,
        }
    }
}

/// Opus encoder configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpusConfig {
    /// Bitrate in bits per second
    pub bitrate: u32,
    
    /// Frame size in samples
    pub frame_size: usize,
    
    /// Sample rate
    pub sample_rate: u32,
    
    /// Number of channels
    pub channels: u16,
    
    /// Application type
    pub application: TrackType,
    
    /// Enable FEC
    pub fec: bool,
    
    /// Expected packet loss percentage (0-100)
    pub packet_loss_perc: u8,
    
    /// Complexity (0-10)
    pub complexity: u8,
    
    /// Enable DTX (discontinuous transmission)
    pub dtx: bool,
    
    /// Enable variable bitrate
    pub vbr: bool,
    
    /// Constrain VBR to not exceed bitrate
    pub cvbr: bool,
    
    /// Signal type hint
    pub signal: OpusSignal,
    
    /// Maximum bandwidth
    pub max_bandwidth: OpusBandwidth,
}

impl Default for OpusConfig {
    fn default() -> Self {
        Self {
            bitrate: DEFAULT_BITRATE,
            frame_size: (DEFAULT_SAMPLE_RATE as f32 * DEFAULT_FRAME_SIZE_MS / 1000.0) as usize,
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            application: TrackType::Music,
            fec: false,
            packet_loss_perc: 0,
            complexity: 10,
            dtx: false,
            vbr: true,
            cvbr: true,
            signal: OpusSignal::Auto,
            max_bandwidth: OpusBandwidth::Fullband,
        }
    }
}

impl OpusConfig {
    /// Create config optimized for voice
    pub fn voice() -> Self {
        Self {
            bitrate: 32_000,
            application: TrackType::Voice,
            fec: true,
            packet_loss_perc: 5,
            complexity: 5,
            dtx: true,
            signal: OpusSignal::Voice,
            max_bandwidth: OpusBandwidth::Wideband,
            ..Default::default()
        }
    }
    
    /// Create config optimized for music
    pub fn music() -> Self {
        Self {
            bitrate: 128_000,
            application: TrackType::Music,
            fec: false,
            complexity: 10,
            dtx: false,
            signal: OpusSignal::Music,
            max_bandwidth: OpusBandwidth::Fullband,
            ..Default::default()
        }
    }
    
    /// Create config optimized for low latency
    pub fn low_latency() -> Self {
        Self {
            bitrate: 96_000,
            frame_size: 120, // 2.5ms at 48kHz
            application: TrackType::LowLatency,
            fec: false,
            complexity: 5, // Lower for faster encoding
            dtx: false,
            vbr: false, // CBR for consistent timing
            ..Default::default()
        }
    }
    
    /// Calculate frame size in samples from milliseconds
    pub fn frame_size_from_ms(sample_rate: u32, ms: f32) -> usize {
        (sample_rate as f32 * ms / 1000.0) as usize
    }
    
    /// Get frame duration in milliseconds
    pub fn frame_duration_ms(&self) -> f32 {
        self.frame_size as f32 * 1000.0 / self.sample_rate as f32
    }
}

/// Opus signal type hint
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpusSignal {
    Auto,
    Voice,
    Music,
}

/// Opus bandwidth
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpusBandwidth {
    /// 4 kHz
    Narrowband,
    /// 6 kHz
    Mediumband,
    /// 8 kHz
    Wideband,
    /// 12 kHz
    Superwideband,
    /// 20 kHz
    Fullband,
}

impl AppConfig {
    /// Load configuration from file
    pub fn load(path: &PathBuf) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| crate::Error::Config(e.to_string()))?;
        Ok(config)
    }
    
    /// Save configuration to file
    pub fn save(&self, path: &PathBuf) -> crate::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::Config(e.to_string()))?;
        std::fs::write(path, content)?;
        Ok(())
    }
    
    /// Get default config file path
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "audio-streamer", "lan-audio")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }
}
