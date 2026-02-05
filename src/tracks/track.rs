//! Представление отдельного аудио-трека
//!
//! Каждый трек имеет собственный измеритель уровня со сглаживанием,
//! что обеспечивает плавную визуализацию в UI без дёрганий.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::audio::buffer::{create_shared_buffer, SharedRingBuffer};
use crate::audio::level_meter::SmoothLevelMeter;
use crate::config::OpusConfig;
use crate::error::TrackError;
use crate::protocol::{TrackConfig, TrackStatus, TrackType};
use crate::constants::RING_BUFFER_CAPACITY;

/// Состояние трека
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    /// Трек создан, но не запущен
    Stopped,
    /// Трек запускается
    Starting,
    /// Трек работает
    Running,
    /// Трек останавливается
    Stopping,
    /// Трек в состоянии ошибки
    Error,
}

/// Аудио-трек (отправитель или получатель)
/// 
/// Примечание: Кодеры/декодеры НЕ хранятся в Track для потокобезопасности.
/// Они должны создаваться и управляться отдельно в пайплайне обработки аудио.
pub struct Track {
    /// ID трека
    pub id: u8,
    
    /// Человекочитаемое имя
    pub name: String,
    
    /// ID устройства (вход для отправителя, выход для получателя)
    pub device_id: String,
    
    /// Конфигурация трека
    pub config: TrackConfig,
    
    /// Текущее состояние
    state: TrackState,
    
    /// Флаг приглушения
    muted: Arc<AtomicBool>,
    
    /// Флаг соло
    solo: Arc<AtomicBool>,
    
    /// Аудио-буфер
    pub buffer: SharedRingBuffer,
    
    /// Счётчик отправленных/полученных пакетов
    packets_count: Arc<AtomicU64>,
    
    /// Счётчик потерянных пакетов
    packets_lost: Arc<AtomicU64>,
    
    /// Текущая задержка в микросекундах (AtomicU32 для потокобезопасности)
    latency_us: Arc<AtomicU32>,
    
    /// Текущая оценка джиттера в микросекундах (AtomicU32 для потокобезопасности)
    jitter_us: Arc<AtomicU32>,
    
    /// Время запуска
    start_time: Option<Instant>,
    
    /// Последнее сообщение об ошибке
    last_error: Option<String>,
    
    /// Сглаженный измеритель уровня (заменяет peak_level_millibels)
    /// Использует lock-free атомарные операции для плавной визуализации
    level_meter: Arc<SmoothLevelMeter>,
}

// Track теперь Send + Sync безопасен (нет сырых указателей)
unsafe impl Send for Track {}
unsafe impl Sync for Track {}

impl Track {
    /// Создать новый трек
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
            // Используем новый сглаженный измеритель уровня
            level_meter: Arc::new(SmoothLevelMeter::new()),
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
    
    /// Получить количество потерянных пакетов
    pub fn packets_lost(&self) -> u64 {
        self.packets_lost.load(Ordering::Relaxed)
    }
    
    /// Обновить уровень из семплов (устаревший метод, для совместимости)
    /// 
    /// ПРИМЕЧАНИЕ: Теперь используется сглаженный измеритель уровня,
    /// который обеспечивает плавную визуализацию без дёрганий.
    pub fn update_level(&mut self, samples: &[f32]) {
        self.level_meter.update_from_samples(samples);
    }
    
    /// Потокобезопасное обновление уровня (вызывается из аудио-потока)
    /// 
    /// Эта функция lock-free и безопасна для real-time контекста.
    /// Использует экспоненциальное сглаживание с учётом времени.
    pub fn update_level_atomic(&self, samples: &[f32]) {
        self.level_meter.update_from_samples(samples);
    }
    
    /// Получить текущий уровень в dB (сглаженный)
    /// 
    /// Возвращает плавно интерполированное значение, подходящее для UI.
    pub fn level_db(&self) -> f32 {
        // Обновляем состояние для UI (затухание без новых данных)
        self.level_meter.tick_for_ui();
        self.level_meter.level_db()
    }
    
    /// Получить пиковый уровень в dB
    pub fn peak_db(&self) -> f32 {
        self.level_meter.peak_db()
    }
    
    /// Получить нормализованный уровень (0.0 - 1.0)
    pub fn level_normalized(&self) -> f32 {
        self.level_meter.tick_for_ui();
        self.level_meter.level_normalized()
    }
    
    /// Получить нормализованный пик (0.0 - 1.0)
    pub fn peak_normalized(&self) -> f32 {
        self.level_meter.peak_normalized()
    }
    
    /// Получить ссылку на измеритель уровня
    pub fn level_meter(&self) -> &Arc<SmoothLevelMeter> {
        &self.level_meter
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
    
    /// Получить последнюю ошибку
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    
    /// Обновить конфигурацию трека
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
            // Примечание: Если кодер существует в другом месте, вызывающий код должен его обновить
        }
        
        if let Some(frame_size_ms) = update.frame_size_ms {
            self.config.frame_size_ms = frame_size_ms;
            // Примечание: Изменение размера кадра требует пересоздания кодера
        }
        
        if let Some(fec) = update.fec_enabled {
            self.config.fec_enabled = fec;
            // Примечание: Если кодер существует в другом месте, вызывающий код должен его обновить
        }
        
        Ok(())
    }
    
    /// Получить статус трека для отчётности
    /// 
    /// Включает сглаженные значения уровня и пика для плавного отображения в UI.
    pub fn status(&self) -> TrackStatus {
        // Обновляем измеритель для плавной анимации
        self.level_meter.tick_for_ui();
        
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
            // Сглаженные значения для плавной визуализации
            level_db: self.level_meter.level_db(),
            peak_db: self.level_meter.peak_db(),
            level_normalized: self.level_meter.level_normalized(),
            peak_normalized: self.level_meter.peak_normalized(),
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        self.stop();
    }
}
