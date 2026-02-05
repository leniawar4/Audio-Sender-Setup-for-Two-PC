//! Модуль аудио-подсистемы
//!
//! Содержит компоненты для захвата, воспроизведения и измерения аудио.

pub mod capture;
pub mod playback;
pub mod buffer;
pub mod device;
pub mod level_meter;

pub use capture::AudioCapture;
pub use playback::AudioPlayback;
pub use buffer::RingBuffer;
pub use device::{list_devices, get_device_by_id, AudioDevice};
pub use level_meter::{SmoothLevelMeter, MultiChannelLevelMeter, LevelMeterParams};
