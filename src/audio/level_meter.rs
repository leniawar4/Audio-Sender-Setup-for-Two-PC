//! Сглаженный измеритель уровня аудио
//!
//! Этот модуль предоставляет потокобезопасный измеритель уровня с плавной
//! анимацией, используя экспоненциальное сглаживание и интерполяцию.
//!
//! ## Проблема исходной реализации
//!
//! Исходная реализация страдала от "дёрганья" UI по следующим причинам:
//! 1. Прямое обновление значений без сглаживания во времени
//! 2. Отсутствие разделения между аудио-потоком и UI-потоком
//! 3. Слишком агрессивный коэффициент сглаживания (0.9/0.1)
//! 4. Нет учёта времени между кадрами (frame-rate independent smoothing)
//!
//! ## Решение
//!
//! Новая реализация использует:
//! - Экспоненциальное сглаживание с учётом времени (time-based exponential smoothing)
//! - Раздельные коэффициенты для атаки и затухания (attack/release)
//! - Линейную интерполяцию (lerp) для плавных переходов
//! - Атомарные операции для безблокировочного доступа из разных потоков
//! - Пиковый индикатор с плавным затуханием

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Параметры сглаживания измерителя уровня
#[derive(Debug, Clone, Copy)]
pub struct LevelMeterParams {
    /// Время атаки в миллисекундах (как быстро уровень поднимается)
    /// Меньшее значение = более быстрая реакция на громкие звуки
    pub attack_ms: f32,
    
    /// Время затухания в миллисекундах (как быстро уровень опускается)
    /// Большее значение = более плавное затухание
    pub release_ms: f32,
    
    /// Время удержания пика в миллисекундах
    /// Пик остаётся на месте это время перед началом затухания
    pub peak_hold_ms: f32,
    
    /// Время затухания пика в миллисекундах
    pub peak_release_ms: f32,
    
    /// Минимальный уровень в dB (порог тишины)
    pub floor_db: f32,
    
    /// Максимальный уровень в dB (0 dB = полная шкала)
    pub ceiling_db: f32,
}

impl Default for LevelMeterParams {
    fn default() -> Self {
        Self {
            // Быстрая атака для точного отображения пиков
            attack_ms: 5.0,
            // Плавное затухание для визуального комфорта
            release_ms: 150.0,
            // Удержание пика для наглядности
            peak_hold_ms: 500.0,
            // Плавное затухание пика
            peak_release_ms: 300.0,
            // Минимум -96 dB (практически тишина)
            floor_db: -96.0,
            // Максимум 0 dB (цифровой потолок)
            ceiling_db: 0.0,
        }
    }
}

/// Внутреннее состояние измерителя уровня
/// Упаковано в u64 для атомарного доступа
#[derive(Debug, Clone, Copy)]
struct LevelState {
    /// Текущий сглаженный уровень в миллибелах (centibels * 10)
    /// Хранится как i32 + 96000 для положительного диапазона
    level_millibels: i32,
    
    /// Пиковый уровень в миллибелах
    peak_millibels: i32,
}

impl LevelState {
    /// Упаковать состояние в u64
    fn pack(self) -> u64 {
        // Смещаем значения в положительный диапазон (добавляем 100000)
        let level = (self.level_millibels + 100_000) as u32;
        let peak = (self.peak_millibels + 100_000) as u32;
        ((level as u64) << 32) | (peak as u64)
    }
    
    /// Распаковать состояние из u64
    fn unpack(packed: u64) -> Self {
        let level = ((packed >> 32) as u32) as i32 - 100_000;
        let peak = (packed as u32) as i32 - 100_000;
        Self {
            level_millibels: level,
            peak_millibels: peak,
        }
    }
    
    /// Получить уровень в dB
    fn level_db(&self) -> f32 {
        self.level_millibels as f32 / 1000.0
    }
    
    /// Получить пиковый уровень в dB
    fn peak_db(&self) -> f32 {
        self.peak_millibels as f32 / 1000.0
    }
}

/// Потокобезопасный измеритель уровня со сглаживанием
/// 
/// Использует lock-free атомарные операции для безопасного доступа
/// из аудио-потока (запись) и UI-потока (чтение).
pub struct SmoothLevelMeter {
    /// Атомарное упакованное состояние
    state: AtomicU64,
    
    /// Параметры сглаживания
    params: LevelMeterParams,
    
    /// Время последнего обновления (в микросекундах от старта)
    last_update_us: AtomicU64,
    
    /// Время последнего пика (в микросекундах от старта)
    last_peak_us: AtomicU64,
    
    /// Время старта для относительных вычислений
    start_time: Instant,
}

impl SmoothLevelMeter {
    /// Создать новый измеритель с параметрами по умолчанию
    pub fn new() -> Self {
        Self::with_params(LevelMeterParams::default())
    }
    
    /// Создать новый измеритель с заданными параметрами
    pub fn with_params(params: LevelMeterParams) -> Self {
        let initial_state = LevelState {
            level_millibels: (params.floor_db * 1000.0) as i32,
            peak_millibels: (params.floor_db * 1000.0) as i32,
        };
        
        Self {
            state: AtomicU64::new(initial_state.pack()),
            params,
            last_update_us: AtomicU64::new(0),
            last_peak_us: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }
    
    /// Получить текущее время в микросекундах
    fn current_time_us(&self) -> u64 {
        self.start_time.elapsed().as_micros() as u64
    }
    
    /// Обновить уровень новыми семплами (вызывается из аудио-потока)
    /// 
    /// Эта функция lock-free и безопасна для вызова из real-time контекста.
    pub fn update_from_samples(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        
        // Вычисляем пиковый уровень входного сигнала
        let peak_amplitude = samples.iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        
        // Конвертируем в dB
        let input_db = if peak_amplitude > 1e-10 {
            20.0 * peak_amplitude.log10()
        } else {
            self.params.floor_db
        };
        
        // Ограничиваем диапазон
        let input_db = input_db.clamp(self.params.floor_db, self.params.ceiling_db);
        let input_millibels = (input_db * 1000.0) as i32;
        
        let now_us = self.current_time_us();
        let last_us = self.last_update_us.load(Ordering::Relaxed);
        
        // Вычисляем дельту времени
        let delta_ms = if last_us > 0 {
            (now_us.saturating_sub(last_us)) as f32 / 1000.0
        } else {
            10.0 // Предполагаем 10ms для первого вызова
        };
        
        // Загружаем текущее состояние
        let current = LevelState::unpack(self.state.load(Ordering::Relaxed));
        
        // Вычисляем коэффициент сглаживания на основе времени
        // Формула: alpha = 1 - e^(-dt/tau), где tau = время сглаживания
        let (attack_alpha, release_alpha) = self.compute_smoothing_alphas(delta_ms);
        
        // Выбираем коэффициент в зависимости от направления изменения
        let alpha = if input_millibels > current.level_millibels {
            attack_alpha // Быстрая атака
        } else {
            release_alpha // Медленное затухание
        };
        
        // Экспоненциальное сглаживание (EMA)
        let new_level = lerp_i32(current.level_millibels, input_millibels, alpha);
        
        // Обновляем пик
        let last_peak_us = self.last_peak_us.load(Ordering::Relaxed);
        let peak_age_ms = (now_us.saturating_sub(last_peak_us)) as f32 / 1000.0;
        
        let new_peak = if input_millibels > current.peak_millibels {
            // Новый пик обнаружен
            self.last_peak_us.store(now_us, Ordering::Relaxed);
            input_millibels
        } else if peak_age_ms > self.params.peak_hold_ms {
            // Время удержания истекло, начинаем затухание
            let peak_alpha = compute_alpha(delta_ms, self.params.peak_release_ms);
            let floor_millibels = (self.params.floor_db * 1000.0) as i32;
            lerp_i32(current.peak_millibels, floor_millibels, peak_alpha)
        } else {
            // Удерживаем пик
            current.peak_millibels
        };
        
        // Сохраняем новое состояние
        let new_state = LevelState {
            level_millibels: new_level,
            peak_millibels: new_peak,
        };
        
        self.state.store(new_state.pack(), Ordering::Relaxed);
        self.last_update_us.store(now_us, Ordering::Relaxed);
    }
    
    /// Вычислить коэффициенты сглаживания для атаки и затухания
    fn compute_smoothing_alphas(&self, delta_ms: f32) -> (f32, f32) {
        let attack_alpha = compute_alpha(delta_ms, self.params.attack_ms);
        let release_alpha = compute_alpha(delta_ms, self.params.release_ms);
        (attack_alpha, release_alpha)
    }
    
    /// Получить текущий сглаженный уровень в dB (вызывается из UI-потока)
    pub fn level_db(&self) -> f32 {
        let state = LevelState::unpack(self.state.load(Ordering::Relaxed));
        state.level_db()
    }
    
    /// Получить текущий пиковый уровень в dB
    pub fn peak_db(&self) -> f32 {
        let state = LevelState::unpack(self.state.load(Ordering::Relaxed));
        state.peak_db()
    }
    
    /// Получить уровень нормализованный к диапазону 0.0-1.0
    pub fn level_normalized(&self) -> f32 {
        let db = self.level_db();
        let range = self.params.ceiling_db - self.params.floor_db;
        ((db - self.params.floor_db) / range).clamp(0.0, 1.0)
    }
    
    /// Получить пик нормализованный к диапазону 0.0-1.0
    pub fn peak_normalized(&self) -> f32 {
        let db = self.peak_db();
        let range = self.params.ceiling_db - self.params.floor_db;
        ((db - self.params.floor_db) / range).clamp(0.0, 1.0)
    }
    
    /// Сбросить измеритель к начальному состоянию
    pub fn reset(&self) {
        let initial_state = LevelState {
            level_millibels: (self.params.floor_db * 1000.0) as i32,
            peak_millibels: (self.params.floor_db * 1000.0) as i32,
        };
        self.state.store(initial_state.pack(), Ordering::Relaxed);
        self.last_update_us.store(0, Ordering::Relaxed);
        self.last_peak_us.store(0, Ordering::Relaxed);
    }
    
    /// Обновить состояние для UI (вызвать перед чтением для плавной анимации)
    /// 
    /// Эта функция продолжает затухание даже без новых семплов,
    /// обеспечивая плавную анимацию в UI.
    pub fn tick_for_ui(&self) {
        let now_us = self.current_time_us();
        let last_us = self.last_update_us.load(Ordering::Relaxed);
        
        if last_us == 0 {
            return; // Ещё не было обновлений
        }
        
        let delta_ms = (now_us.saturating_sub(last_us)) as f32 / 1000.0;
        
        // Если прошло больше 50ms без обновлений, продолжаем затухание
        if delta_ms > 50.0 {
            let current = LevelState::unpack(self.state.load(Ordering::Relaxed));
            let floor_millibels = (self.params.floor_db * 1000.0) as i32;
            
            // Затухаем к минимуму
            let release_alpha = compute_alpha(delta_ms.min(100.0), self.params.release_ms);
            let new_level = lerp_i32(current.level_millibels, floor_millibels, release_alpha);
            
            // Также затухаем пик если время удержания истекло
            let last_peak_us = self.last_peak_us.load(Ordering::Relaxed);
            let peak_age_ms = (now_us.saturating_sub(last_peak_us)) as f32 / 1000.0;
            
            let new_peak = if peak_age_ms > self.params.peak_hold_ms {
                let peak_alpha = compute_alpha(delta_ms.min(100.0), self.params.peak_release_ms);
                lerp_i32(current.peak_millibels, floor_millibels, peak_alpha)
            } else {
                current.peak_millibels
            };
            
            let new_state = LevelState {
                level_millibels: new_level,
                peak_millibels: new_peak,
            };
            
            self.state.store(new_state.pack(), Ordering::Relaxed);
        }
    }
}

impl Default for SmoothLevelMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Вычислить коэффициент сглаживания alpha для экспоненциального фильтра
/// 
/// Формула: alpha = 1 - e^(-dt/tau)
/// Где dt - дельта времени, tau - постоянная времени (время сглаживания)
#[inline]
fn compute_alpha(delta_ms: f32, time_constant_ms: f32) -> f32 {
    if time_constant_ms <= 0.0 {
        return 1.0; // Мгновенное изменение
    }
    1.0 - (-delta_ms / time_constant_ms).exp()
}

/// Линейная интерполяция для целых чисел
#[inline]
fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    let t = t.clamp(0.0, 1.0);
    (a as f32 + (b - a) as f32 * t) as i32
}

/// Мульти-канальный измеритель уровня (для стерео и более)
pub struct MultiChannelLevelMeter {
    /// Измерители для каждого канала
    channels: Vec<SmoothLevelMeter>,
    
    /// Комбинированный измеритель (максимум всех каналов)
    combined: SmoothLevelMeter,
}

impl MultiChannelLevelMeter {
    /// Создать измеритель для заданного количества каналов
    pub fn new(channel_count: usize) -> Self {
        let params = LevelMeterParams::default();
        Self::with_params(channel_count, params)
    }
    
    /// Создать измеритель с заданными параметрами
    pub fn with_params(channel_count: usize, params: LevelMeterParams) -> Self {
        let channels = (0..channel_count)
            .map(|_| SmoothLevelMeter::with_params(params))
            .collect();
        
        Self {
            channels,
            combined: SmoothLevelMeter::with_params(params),
        }
    }
    
    /// Обновить из interleaved семплов (L, R, L, R, ...)
    pub fn update_interleaved(&self, samples: &[f32], channel_count: usize) {
        if samples.is_empty() || channel_count == 0 {
            return;
        }
        
        // Обновляем каждый канал отдельно
        for (ch, meter) in self.channels.iter().enumerate() {
            if ch < channel_count {
                // Извлекаем семплы для этого канала
                let channel_samples: Vec<f32> = samples.iter()
                    .skip(ch)
                    .step_by(channel_count)
                    .copied()
                    .collect();
                
                meter.update_from_samples(&channel_samples);
            }
        }
        
        // Обновляем комбинированный измеритель (для всех семплов)
        self.combined.update_from_samples(samples);
    }
    
    /// Получить уровень канала в dB
    pub fn channel_level_db(&self, channel: usize) -> f32 {
        self.channels.get(channel)
            .map(|m| m.level_db())
            .unwrap_or(-96.0)
    }
    
    /// Получить пик канала в dB
    pub fn channel_peak_db(&self, channel: usize) -> f32 {
        self.channels.get(channel)
            .map(|m| m.peak_db())
            .unwrap_or(-96.0)
    }
    
    /// Получить комбинированный уровень в dB
    pub fn combined_level_db(&self) -> f32 {
        self.combined.level_db()
    }
    
    /// Получить комбинированный пик в dB
    pub fn combined_peak_db(&self) -> f32 {
        self.combined.peak_db()
    }
    
    /// Количество каналов
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }
    
    /// Обновить для UI (tick для плавной анимации без новых данных)
    pub fn tick_for_ui(&self) {
        for meter in &self.channels {
            meter.tick_for_ui();
        }
        self.combined.tick_for_ui();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_level_state_pack_unpack() {
        let state = LevelState {
            level_millibels: -48000,
            peak_millibels: -12000,
        };
        
        let packed = state.pack();
        let unpacked = LevelState::unpack(packed);
        
        assert_eq!(state.level_millibels, unpacked.level_millibels);
        assert_eq!(state.peak_millibels, unpacked.peak_millibels);
    }
    
    #[test]
    fn test_compute_alpha() {
        // При delta = tau, alpha должен быть примерно 0.632
        let alpha = compute_alpha(100.0, 100.0);
        assert!((alpha - 0.632).abs() < 0.01);
        
        // При очень маленькой delta, alpha близок к 0
        let alpha_small = compute_alpha(1.0, 100.0);
        assert!(alpha_small < 0.02);
        
        // При большой delta, alpha близок к 1
        let alpha_large = compute_alpha(1000.0, 100.0);
        assert!(alpha_large > 0.99);
    }
    
    #[test]
    fn test_smooth_level_meter_basic() {
        let meter = SmoothLevelMeter::new();
        
        // Изначально уровень должен быть минимальным
        assert!(meter.level_db() <= -90.0);
        
        // Обновляем с громким сигналом
        let loud_samples: Vec<f32> = (0..480).map(|i| {
            0.5 * (i as f32 * 0.1).sin()
        }).collect();
        
        meter.update_from_samples(&loud_samples);
        
        // Уровень должен подняться
        assert!(meter.level_db() > -96.0);
    }
    
    #[test]
    fn test_lerp_i32() {
        assert_eq!(lerp_i32(0, 100, 0.0), 0);
        assert_eq!(lerp_i32(0, 100, 1.0), 100);
        assert_eq!(lerp_i32(0, 100, 0.5), 50);
        assert_eq!(lerp_i32(-100, 100, 0.5), 0);
    }
}
