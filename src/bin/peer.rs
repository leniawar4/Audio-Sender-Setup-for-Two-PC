//! Единое P2P приложение для двунаправленной передачи аудио
//!
//! Это приложение объединяет функциональность отправителя и получателя
//! в единый симметричный пир, позволяя:
//! - Отправлять аудио другим пирам
//! - Получать аудио от других пиров  
//! - Работать одновременно в обоих направлениях
//! - Автоматически обнаруживать и синхронизироваться с пирами
//!
//! ## Архитектура
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           PEER APPLICATION                               │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │                      TRACK MANAGER                                │  │
//! │  │  ┌─────────────────┐         ┌─────────────────┐                │  │
//! │  │  │   INPUT TRACKS   │         │  OUTPUT TRACKS   │                │  │
//! │  │  │  (для отправки)  │         │ (для получения)  │                │  │
//! │  │  └─────────────────┘         └─────────────────┘                │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                    │                                    │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │                      PEER NETWORK                                 │  │
//! │  │  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────┐  │  │
//! │  │  │   SENDER    │    │  RECEIVER   │    │    DISCOVERY        │  │  │
//! │  │  │   (UDP)     │    │   (UDP)     │    │    (UDP Broadcast)  │  │  │
//! │  │  └─────────────┘    └─────────────┘    └─────────────────────┘  │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                    │                                    │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │                         WEB UI                                    │  │
//! │  │   HTTP сервер + WebSocket для управления и мониторинга           │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```

use anyhow::Result;
use crossbeam_channel::bounded;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use lan_audio_streamer::{
    audio::{
        buffer::{create_shared_buffer, AudioFrame, JitterBuffer, SharedRingBuffer},
        capture::AudioCapture,
        device::list_devices,
        playback::NetworkPlayback,
    },
    codec::{OpusDecoder, OpusEncoder},
    config::{AppConfig, OpusConfig},
    constants::*,
    network::{
        discovery::{DiscoveredPeer, DiscoveryService, get_best_local_address, get_local_addresses},
        receiver::{AudioReceiver, ReceivedPacket},
        sender::MultiTrackSender,
    },
    protocol::TrackConfig,
    tracks::{TrackEvent, TrackManager},
    ui::WebServer,
};

/// Состояние входящего трека (для отправки аудио)
struct InputTrackState {
    capture: AudioCapture,
    capture_buffer: SharedRingBuffer,
    encoder: OpusEncoder,
    sample_buffer: Vec<f32>,
    sequence: u32,
}

/// Состояние выходящего трека (для получения аудио)
#[allow(dead_code)]
struct OutputTrackState {
    decoder: OpusDecoder,
    jitter_buffer: JitterBuffer,
    playback: Option<NetworkPlayback>,
    packets_received: u64,
    packets_lost: u64,
    device_id: String,
    channels: u16,
}

/// Информация о подключённом пире
#[derive(Debug, Clone)]
struct ConnectedPeer {
    /// Адрес для отправки аудио
    send_address: SocketAddr,
    /// Имя пира
    name: String,
    /// Время последней активности
    last_seen: Instant,
    /// Активен ли пир
    active: bool,
}

/// Конфигурация пира
#[derive(Debug, Clone)]
struct PeerConfig {
    /// Имя этого пира (отображается другим пирам)
    name: String,
    /// Предпочтительный порт для аудио
    preferred_port: u16,
    /// Автоматическое подключение к обнаруженным пирам
    auto_connect: bool,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            name: format!("Peer-{}", std::process::id()),
            preferred_port: DEFAULT_UDP_PORT,
            auto_connect: true,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Инициализация логирования
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();
    
    tracing::info!("═══════════════════════════════════════════════════════════════");
    tracing::info!("       LAN Audio Streamer - Bidirectional Peer Application     ");
    tracing::info!("═══════════════════════════════════════════════════════════════");
    
    // Загружаем конфигурацию
    let mut config = AppConfig::default();
    let peer_config = parse_args();
    
    // Определяем доступный порт
    let audio_port = find_available_port(peer_config.preferred_port)?;
    config.network.udp_port = audio_port;
    
    tracing::info!("Имя пира: {}", peer_config.name);
    tracing::info!("Аудио порт: {}", audio_port);
    
    // Выводим список устройств
    print_devices();
    
    // Выводим локальные адреса
    print_local_addresses(audio_port);
    
    // Создаём менеджер треков (общий для входящих и выходящих)
    let track_manager = Arc::new(TrackManager::new());
    
    // Подписываемся на события треков
    let mut event_rx = track_manager.subscribe();
    
    // Запускаем веб-интерфейс
    let web_server = WebServer::new(
        config.ui.clone(),
        track_manager.clone(),
        true, // is_sender - показываем обе функции
    );
    let _web_handle = web_server.start_background();
    
    tracing::info!(
        "Web UI доступен: http://{}:{}",
        config.ui.bind_address,
        config.ui.http_port
    );
    
    // Создаём и запускаем сервис обнаружения
    let peers: Arc<Mutex<HashMap<String, ConnectedPeer>>> = Arc::new(Mutex::new(HashMap::new()));
    let peers_for_discovery = peers.clone();
    
    let mut discovery = DiscoveryService::new(
        true, // Оба режима - и отправитель, и получатель
        audio_port,
        peer_config.name.clone(),
    );
    
    // Обрабатываем обнаруженные пиры
    discovery.on_peer_discovered(move |peer| {
        handle_peer_discovered(&peers_for_discovery, peer, peer_config.auto_connect);
    });
    
    if let Err(e) = discovery.start() {
        tracing::warn!("Не удалось запустить сервис обнаружения: {}", e);
    } else {
        tracing::info!("Сервис обнаружения запущен");
    }
    
    // Создаём канал для приёма пакетов
    let (packet_tx, packet_rx) = bounded::<ReceivedPacket>(4096);
    
    // Запускаем сетевой приёмник
    let mut receiver = AudioReceiver::new();
    receiver.set_global_channel(packet_tx);
    receiver.start(config.network.clone())?;
    tracing::info!("Сетевой приёмник запущен на порту {}", config.network.udp_port);
    
    // Состояния треков
    let input_states: Arc<Mutex<HashMap<u8, InputTrackState>>> = Arc::new(Mutex::new(HashMap::new()));
    let output_states: Arc<Mutex<HashMap<u8, OutputTrackState>>> = Arc::new(Mutex::new(HashMap::new()));
    
    // Множество удалённых треков (не пересоздавать автоматически)
    let deleted_output_tracks: Arc<Mutex<HashSet<u8>>> = Arc::new(Mutex::new(HashSet::new()));
    
    // Получаем устройство вывода по умолчанию
    let devices = list_devices();
    let default_output = devices
        .iter()
        .find(|d| d.is_output && d.is_default)
        .map(|d| d.id.clone())
        .unwrap_or_default();
    
    tracing::info!("Устройство вывода по умолчанию: {}", default_output);
    
    // Клонируем для обработчика событий
    let input_states_for_events = input_states.clone();
    let track_manager_for_events = track_manager.clone();
    
    // Обработчик событий треков
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    handle_track_event(
                        event,
                        &input_states_for_events,
                        &track_manager_for_events,
                    );
                }
                Err(e) => {
                    tracing::warn!("Ошибка канала событий: {}", e);
                }
            }
        }
    });
    
    // Создаём сетевой отправитель (будет обновляться при обнаружении пиров)
    let network_senders: Arc<Mutex<HashMap<String, MultiTrackSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let peers_for_main = peers.clone();
    let network_senders_for_main = network_senders.clone();
    
    // Флаг работы
    let running = Arc::new(AtomicBool::new(true));
    let running_for_signal = running.clone();
    
    // Обработчик сигнала завершения
    ctrlc_handler(running_for_signal);
    
    let start_time = Instant::now();
    let mut last_stats_time = Instant::now();
    let mut last_peer_check_time = Instant::now();
    
    tracing::info!("Запуск основного цикла - нажмите Ctrl+C для остановки");
    
    // Основной цикл
    while running.load(Ordering::Relaxed) {
        // Периодическая проверка пиров и создание отправителей
        if last_peer_check_time.elapsed() >= Duration::from_secs(1) {
            last_peer_check_time = Instant::now();
            update_peer_connections(
                &peers_for_main,
                &network_senders_for_main,
                &config.network,
            );
        }
        
        // Обрабатываем входящие треки (отправка)
        let has_send_work = process_input_tracks(
            &input_states,
            &track_manager,
            &network_senders,
            start_time,
        );
        
        // Обрабатываем входящие пакеты (получение)
        let has_recv_work = process_received_packets(
            &packet_rx,
            &output_states,
            &deleted_output_tracks,
            &track_manager,
            &default_output,
        );
        
        // Адаптивный сон
        if has_send_work || has_recv_work {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(Duration::from_micros(250)).await;
        }
        
        // Периодическая статистика
        if last_stats_time.elapsed() >= Duration::from_secs(5) {
            last_stats_time = Instant::now();
            print_stats(&input_states, &output_states, &peers_for_main, &receiver);
        }
    }
    
    tracing::info!("Завершение работы...");
    discovery.stop();
    receiver.stop();
    
    Ok(())
}

/// Разбор аргументов командной строки
fn parse_args() -> PeerConfig {
    let mut config = PeerConfig::default();
    
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    
    while i < args.len() {
        match args[i].as_str() {
            "--name" | "-n" => {
                if i + 1 < args.len() {
                    config.name = args[i + 1].clone();
                    i += 1;
                }
            }
            "--port" | "-p" => {
                if i + 1 < args.len() {
                    if let Ok(port) = args[i + 1].parse() {
                        config.preferred_port = port;
                    }
                    i += 1;
                }
            }
            "--no-auto-connect" => {
                config.auto_connect = false;
            }
            "--help" | "-h" => {
                println!("LAN Audio Streamer - Bidirectional Peer Application");
                println!();
                println!("Использование: peer [ОПЦИИ]");
                println!();
                println!("Опции:");
                println!("  -n, --name <ИМЯ>      Имя пира (по умолчанию: Peer-<PID>)");
                println!("  -p, --port <ПОРТ>     Предпочтительный порт (по умолчанию: 5000)");
                println!("  --no-auto-connect     Не подключаться автоматически к пирам");
                println!("  -h, --help            Показать справку");
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }
    
    config
}

/// Найти доступный порт
fn find_available_port(preferred: u16) -> Result<u16> {
    use std::net::UdpSocket;
    
    // Сначала пробуем предпочтительный порт
    if UdpSocket::bind(format!("0.0.0.0:{}", preferred)).is_ok() {
        return Ok(preferred);
    }
    
    tracing::warn!("Порт {} занят, ищем свободный порт...", preferred);
    
    // Ищем свободный порт в диапазоне
    for port in (preferred + 1)..=(preferred + 100) {
        if UdpSocket::bind(format!("0.0.0.0:{}", port)).is_ok() {
            return Ok(port);
        }
    }
    
    // Последняя попытка - любой свободный порт
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let port = socket.local_addr()?.port();
    drop(socket);
    
    Ok(port)
}

/// Вывести список устройств
fn print_devices() {
    let devices = list_devices();
    
    println!();
    println!("╔════════════════════════════════════════════════════════════════════╗");
    println!("║                      ДОСТУПНЫЕ АУДИО-УСТРОЙСТВА                    ║");
    println!("╠════════════════════════════════════════════════════════════════════╣");
    
    for device in &devices {
        let device_type = match (device.is_input, device.is_output) {
            (true, true) => "Вход/Выход",
            (true, false) => "Вход     ",
            (false, true) => "Выход    ",
            _ => "Неизвестно",
        };
        let default_marker = if device.is_default { " [ПО УМОЛЧАНИЮ]" } else { "" };
        println!("║ {} {} {}", device_type, device.name, default_marker);
    }
    
    println!("╚════════════════════════════════════════════════════════════════════╝");
    println!();
}

/// Вывести локальные адреса
fn print_local_addresses(port: u16) {
    println!("╔════════════════════════════════════════════════════════════════════╗");
    println!("║                      ЛОКАЛЬНЫЕ СЕТЕВЫЕ АДРЕСА                      ║");
    println!("╠════════════════════════════════════════════════════════════════════╣");
    
    for addr in get_local_addresses() {
        println!("║ {}:{}", addr, port);
    }
    
    if let Some(best) = get_best_local_address() {
        println!("╟────────────────────────────────────────────────────────────────────╢");
        println!("║ Лучший для LAN: {}:{}", best, port);
    }
    
    println!("╚════════════════════════════════════════════════════════════════════╝");
    println!();
}

/// Обработать обнаруженный пир
fn handle_peer_discovered(
    peers: &Arc<Mutex<HashMap<String, ConnectedPeer>>>,
    peer: DiscoveredPeer,
    auto_connect: bool,
) {
    let peer_key = format!("{}:{}", peer.address.ip(), peer.audio_port);
    
    let mut peers_guard = peers.lock();
    
    if !peers_guard.contains_key(&peer_key) {
        tracing::info!(
            "Обнаружен новый пир: {} ({}:{})",
            peer.name,
            peer.address.ip(),
            peer.audio_port
        );
        
        let connected_peer = ConnectedPeer {
            send_address: peer.audio_address(),
            name: peer.name.clone(),
            last_seen: Instant::now(),
            active: auto_connect,
        };
        
        peers_guard.insert(peer_key, connected_peer);
    } else if let Some(existing) = peers_guard.get_mut(&peer_key) {
        existing.last_seen = Instant::now();
    }
}

/// Обновить соединения с пирами
fn update_peer_connections(
    peers: &Arc<Mutex<HashMap<String, ConnectedPeer>>>,
    senders: &Arc<Mutex<HashMap<String, MultiTrackSender>>>,
    network_config: &lan_audio_streamer::config::NetworkConfig,
) {
    let peers_guard = peers.lock();
    let mut senders_guard = senders.lock();
    
    for (key, peer) in peers_guard.iter() {
        if peer.active && !senders_guard.contains_key(key) {
            // Создаём новый отправитель для этого пира
            match MultiTrackSender::new(network_config, peer.send_address) {
                Ok(mut sender) => {
                    if let Err(e) = sender.start(network_config.clone()) {
                        tracing::error!("Не удалось запустить отправитель для {}: {}", key, e);
                    } else {
                        tracing::info!("Создан отправитель для пира {}: {}", peer.name, key);
                        senders_guard.insert(key.clone(), sender);
                    }
                }
                Err(e) => {
                    tracing::error!("Не удалось создать отправитель для {}: {}", key, e);
                }
            }
        }
    }
    
    // Удаляем отправители для неактивных пиров
    let inactive_keys: Vec<String> = senders_guard
        .keys()
        .filter(|k| {
            peers_guard.get(*k).map(|p| !p.active).unwrap_or(true)
        })
        .cloned()
        .collect();
    
    for key in inactive_keys {
        senders_guard.remove(&key);
        tracing::info!("Удалён отправитель для пира: {}", key);
    }
}

/// Обработать событие трека
fn handle_track_event(
    event: TrackEvent,
    input_states: &Arc<Mutex<HashMap<u8, InputTrackState>>>,
    track_manager: &Arc<TrackManager>,
) {
    match event {
        TrackEvent::Created(track_id) => {
            tracing::info!("Трек {} создан, инициализация захвата...", track_id);
            
            if let Some(track) = track_manager.get_track(track_id) {
                let device_id = track.device_id.clone();
                drop(track);
                
                if let Err(e) = create_capture_for_track(track_id, &device_id, input_states) {
                    tracing::error!("Не удалось создать захват для трека {}: {}", track_id, e);
                }
            }
        }
        
        TrackEvent::Removed(track_id) => {
            tracing::info!("Трек {} удалён, остановка захвата...", track_id);
            let mut states = input_states.lock();
            if let Some(mut state) = states.remove(&track_id) {
                state.capture.stop();
                tracing::info!("Захват остановлен для трека {}", track_id);
            }
        }
        
        TrackEvent::DeviceChanged(track_id, old_device, new_device) => {
            tracing::info!(
                "Трек {}: устройство изменено {} -> {}",
                track_id,
                old_device,
                new_device
            );
            
            // Останавливаем старый захват
            {
                let mut states = input_states.lock();
                if let Some(mut state) = states.remove(&track_id) {
                    state.capture.stop();
                }
            }
            
            // Создаём новый захват
            if let Err(e) = create_capture_for_track(track_id, &new_device, input_states) {
                tracing::error!(
                    "Не удалось создать захват для трека {} на устройстве {}: {}",
                    track_id,
                    new_device,
                    e
                );
            }
        }
        
        _ => {}
    }
}

/// Создать захват для трека
fn create_capture_for_track(
    track_id: u8,
    device_id: &str,
    track_states: &Arc<Mutex<HashMap<u8, InputTrackState>>>,
) -> Result<()> {
    let capture_buffer = create_shared_buffer(RING_BUFFER_CAPACITY);
    
    let mut capture = AudioCapture::new(
        track_id,
        device_id,
        Some(DEFAULT_SAMPLE_RATE),
        Some(DEFAULT_CHANNELS),
        None,
        capture_buffer.clone(),
    )?;
    
    capture.start()?;
    tracing::info!("Захват аудио запущен для трека {} на устройстве {}", track_id, device_id);
    
    let opus_config = OpusConfig::music();
    let encoder = OpusEncoder::new(opus_config)?;
    let frame_size = encoder.samples_per_frame();
    
    tracing::info!(
        "Opus кодер инициализирован для трека {}: {}Hz, {} каналов, {} семплов/кадр ({:.1}ms)",
        track_id,
        DEFAULT_SAMPLE_RATE,
        DEFAULT_CHANNELS,
        frame_size,
        encoder.frame_duration_ms()
    );
    
    let state = InputTrackState {
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

/// Обработать входящие треки (отправка)
fn process_input_tracks(
    input_states: &Arc<Mutex<HashMap<u8, InputTrackState>>>,
    track_manager: &Arc<TrackManager>,
    network_senders: &Arc<Mutex<HashMap<String, MultiTrackSender>>>,
    start_time: Instant,
) -> bool {
    let mut states = input_states.lock();
    let mut work_done = false;
    
    for (track_id, state) in states.iter_mut() {
        let frame_size = state.encoder.samples_per_frame();
        
        // Извлекаем все доступные захваченные данные
        while let Some(frame) = state.capture_buffer.try_pop() {
            work_done = true;
            state.sample_buffer.extend_from_slice(&frame.samples);
            
            // Обновляем уровень аудио для трека
            if let Some(track) = track_manager.get_track(*track_id) {
                track.update_level_atomic(&frame.samples);
            }
            
            // Обрабатываем полные кадры
            while state.sample_buffer.len() >= frame_size {
                let samples: Vec<f32> = state.sample_buffer.drain(..frame_size).collect();
                
                match state.encoder.encode(&samples) {
                    Ok(encoded) => {
                        let timestamp = start_time.elapsed().as_micros() as u64;
                        
                        // Отправляем всем подключённым пирам
                        let senders = network_senders.lock();
                        for sender in senders.values() {
                            if let Err(e) = sender.send_audio(
                                *track_id,
                                encoded.clone(),
                                timestamp,
                                DEFAULT_CHANNELS == 2,
                            ) {
                                if state.sequence % 1000 == 0 {
                                    tracing::warn!(
                                        "Не удалось отправить пакет для трека {}: {}",
                                        track_id,
                                        e
                                    );
                                }
                            }
                        }
                        
                        // Обновляем счётчик пакетов
                        if let Some(track) = track_manager.get_track(*track_id) {
                            track.increment_packets();
                            let encode_time_us = (state.encoder.frame_duration_ms() * 1000.0) as u32;
                            track.update_latency(encode_time_us);
                        }
                        
                        state.sequence = state.sequence.wrapping_add(1);
                    }
                    Err(e) => {
                        tracing::warn!("Ошибка кодирования для трека {}: {}", track_id, e);
                    }
                }
            }
        }
    }
    
    work_done
}

/// Обработать полученные пакеты (получение)
fn process_received_packets(
    packet_rx: &crossbeam_channel::Receiver<ReceivedPacket>,
    output_states: &Arc<Mutex<HashMap<u8, OutputTrackState>>>,
    deleted_tracks: &Arc<Mutex<HashSet<u8>>>,
    track_manager: &Arc<TrackManager>,
    default_output: &str,
) -> bool {
    let mut processed_count = 0;
    const MAX_BATCH_SIZE: usize = 64;
    
    while processed_count < MAX_BATCH_SIZE {
        match packet_rx.try_recv() {
            Ok(packet) => {
                processed_count += 1;
                let track_id = packet.track_id;
                
                // Пропускаем пакеты для удалённых треков
                if deleted_tracks.lock().contains(&track_id) {
                    continue;
                }
                
                let mut states = output_states.lock();
                
                // Инициализируем состояние если трек новый
                if !states.contains_key(&track_id) {
                    tracing::info!("Обнаружен новый входящий трек {}, инициализация...", track_id);
                    
                    let channels = if packet.is_stereo { 2 } else { 1 };
                    let output_device = if let Some(track) = track_manager.get_track(track_id) {
                        if !track.device_id.is_empty() {
                            track.device_id.clone()
                        } else {
                            default_output.to_string()
                        }
                    } else {
                        default_output.to_string()
                    };
                    
                    // Создаём декодер
                    let frame_size =
                        (DEFAULT_SAMPLE_RATE as f32 * DEFAULT_FRAME_SIZE_MS / 1000.0) as usize;
                    let decoder = match OpusDecoder::new(DEFAULT_SAMPLE_RATE, channels, frame_size) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(
                                "Не удалось создать декодер для трека {}: {}",
                                track_id,
                                e
                            );
                            continue;
                        }
                    };
                    
                    let jitter_buffer = JitterBuffer::new(32, 2);
                    
                    // Создаём воспроизведение
                    let playback = if !output_device.is_empty() {
                        match NetworkPlayback::new(
                            track_id,
                            &output_device,
                            Some(DEFAULT_SAMPLE_RATE),
                            Some(channels),
                            32,
                            2,
                        ) {
                            Ok(mut p) => {
                                if let Err(e) = p.start() {
                                    tracing::warn!(
                                        "Не удалось запустить воспроизведение для трека {}: {}",
                                        track_id,
                                        e
                                    );
                                    None
                                } else {
                                    tracing::info!(
                                        "Воспроизведение запущено для трека {} на {}",
                                        track_id,
                                        output_device
                                    );
                                    Some(p)
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Не удалось создать воспроизведение для трека {}: {}",
                                    track_id,
                                    e
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    
                    // Создаём трек в менеджере
                    if track_manager.get_track(track_id).is_none() {
                        let track_config = TrackConfig {
                            track_id: Some(track_id),
                            name: format!("Входящий трек {}", track_id),
                            device_id: output_device.clone(),
                            bitrate: DEFAULT_BITRATE,
                            frame_size_ms: DEFAULT_FRAME_SIZE_MS,
                            channels,
                            ..Default::default()
                        };
                        let _ = track_manager.create_track(track_config);
                    }
                    
                    states.insert(
                        track_id,
                        OutputTrackState {
                            decoder,
                            jitter_buffer,
                            playback,
                            packets_received: 0,
                            packets_lost: 0,
                            device_id: output_device,
                            channels,
                        },
                    );
                }
                
                // Обрабатываем пакет
                if let Some(state) = states.get_mut(&track_id) {
                    state.packets_received += 1;
                    
                    if let Some(track) = track_manager.get_track(track_id) {
                        track.increment_packets();
                    }
                    
                    // Декодируем аудио
                    match state.decoder.decode(&packet.payload) {
                        Ok(samples) => {
                            if let Some(track) = track_manager.get_track(track_id) {
                                track.update_level_atomic(&samples);
                            }
                            
                            let frame = AudioFrame::new(
                                samples,
                                state.decoder.channels(),
                                packet.timestamp,
                                packet.sequence,
                            );
                            
                            state.jitter_buffer.insert(frame);
                            
                            // Обновляем метрики
                            let jitter_stats = state.jitter_buffer.stats();
                            if let Some(track) = track_manager.get_track(track_id) {
                                track.update_jitter(jitter_stats.jitter_us as u32);
                                let buffer_latency_us = jitter_stats.target_delay as u32 * 10000;
                                track.update_latency(buffer_latency_us);
                            }
                            
                            // Воспроизводим готовые кадры
                            while let Some(ready_frame) = state.jitter_buffer.get_next() {
                                if let Some(ref playback) = state.playback {
                                    playback.push_frame_direct(ready_frame);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Ошибка декодирования трека {}: {}", track_id, e);
                            state.packets_lost += 1;
                            
                            if let Some(track) = track_manager.get_track(track_id) {
                                track.increment_lost();
                            }
                        }
                    }
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => break,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                tracing::error!("Канал пакетов отключён");
                break;
            }
        }
    }
    
    processed_count > 0
}

/// Вывести статистику
fn print_stats(
    input_states: &Arc<Mutex<HashMap<u8, InputTrackState>>>,
    output_states: &Arc<Mutex<HashMap<u8, OutputTrackState>>>,
    peers: &Arc<Mutex<HashMap<String, ConnectedPeer>>>,
    receiver: &AudioReceiver,
) {
    let input_count = input_states.lock().len();
    let output_count = output_states.lock().len();
    let peer_count = peers.lock().len();
    let recv_stats = receiver.stats();
    
    tracing::info!(
        "Статистика: {} входящих треков, {} выходящих треков, {} пиров, {} принято пакетов",
        input_count,
        output_count,
        peer_count,
        recv_stats.packets_received
    );
}

/// Обработчик Ctrl+C
fn ctrlc_handler(running: Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        tokio::spawn(async move {
            let mut sig = signal(SignalKind::interrupt()).expect("Не удалось настроить обработчик сигнала");
            sig.recv().await;
            running.store(false, Ordering::SeqCst);
        });
    }
    
    #[cfg(windows)]
    {
        let r = running.clone();
        std::thread::spawn(move || {
            let _ = ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            });
        });
    }
}
