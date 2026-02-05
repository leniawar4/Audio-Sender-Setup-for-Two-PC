//! Протокол рукопожатия для синхронизации пиров
//!
//! Этот модуль реализует протокол handshake для:
//! - Обмена информацией о возможностях (capabilities)
//! - Синхронизации состояния между пирами
//! - Обработки конфликтов портов
//! - Согласования параметров соединения
//!
//! ## Протокол
//!
//! ```text
//! Peer A                           Peer B
//!   │                                 │
//!   │──── HELLO (capabilities) ─────>│
//!   │                                 │
//!   │<─── HELLO_ACK (capabilities) ──│
//!   │                                 │
//!   │──── SYNC_REQUEST ─────────────>│
//!   │                                 │
//!   │<─── SYNC_RESPONSE (tracks) ────│
//!   │                                 │
//!   │<───── AUDIO STREAMING ────────>│
//!   │                                 │
//! ```

use bytes::{BufMut, Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Магические байты для пакетов рукопожатия
const HANDSHAKE_MAGIC: &[u8; 4] = b"LAHS"; // LAN Audio HandShake

/// Версия протокола
const PROTOCOL_VERSION: u8 = 1;

/// Типы пакетов рукопожатия
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakePacketType {
    /// Приветствие с информацией о возможностях
    Hello = 0x01,
    /// Подтверждение приветствия
    HelloAck = 0x02,
    /// Запрос синхронизации треков
    SyncRequest = 0x03,
    /// Ответ с информацией о треках
    SyncResponse = 0x04,
    /// Пинг для проверки соединения
    Ping = 0x05,
    /// Понг - ответ на пинг
    Pong = 0x06,
    /// Уведомление об отключении
    Goodbye = 0x07,
    /// Уведомление об ошибке
    ErrorPacket = 0xFF,
}

impl TryFrom<u8> for HandshakePacketType {
    type Error = ();
    
    fn try_from(value: u8) -> Result<Self, <Self as TryFrom<u8>>::Error> {
        match value {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::HelloAck),
            0x03 => Ok(Self::SyncRequest),
            0x04 => Ok(Self::SyncResponse),
            0x05 => Ok(Self::Ping),
            0x06 => Ok(Self::Pong),
            0x07 => Ok(Self::Goodbye),
            0xFF => Ok(Self::ErrorPacket),
            _ => Err(()),
        }
    }
}

/// Флаги возможностей пира
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerCapabilities {
    /// Может отправлять аудио
    pub can_send: bool,
    /// Может принимать аудио
    pub can_receive: bool,
    /// Поддерживает Opus кодек
    pub supports_opus: bool,
    /// Поддерживает FEC (Forward Error Correction)
    pub supports_fec: bool,
    /// Поддерживает стерео
    pub supports_stereo: bool,
    /// Максимальное количество треков
    pub max_tracks: u8,
}

impl PeerCapabilities {
    /// Полные возможности (отправка и приём)
    pub fn full() -> Self {
        Self {
            can_send: true,
            can_receive: true,
            supports_opus: true,
            supports_fec: true,
            supports_stereo: true,
            max_tracks: 16,
        }
    }
    
    /// Только отправка
    pub fn sender_only() -> Self {
        Self {
            can_send: true,
            can_receive: false,
            supports_opus: true,
            supports_fec: true,
            supports_stereo: true,
            max_tracks: 16,
        }
    }
    
    /// Только приём
    pub fn receiver_only() -> Self {
        Self {
            can_send: false,
            can_receive: true,
            supports_opus: true,
            supports_fec: true,
            supports_stereo: true,
            max_tracks: 16,
        }
    }
    
    /// Сериализовать в байты
    pub fn to_bytes(&self) -> [u8; 2] {
        let mut flags = 0u8;
        if self.can_send { flags |= 0x01; }
        if self.can_receive { flags |= 0x02; }
        if self.supports_opus { flags |= 0x04; }
        if self.supports_fec { flags |= 0x08; }
        if self.supports_stereo { flags |= 0x10; }
        
        [flags, self.max_tracks]
    }
    
    /// Десериализовать из байтов
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 2 {
            return None;
        }
        
        let flags = data[0];
        Some(Self {
            can_send: flags & 0x01 != 0,
            can_receive: flags & 0x02 != 0,
            supports_opus: flags & 0x04 != 0,
            supports_fec: flags & 0x08 != 0,
            supports_stereo: flags & 0x10 != 0,
            max_tracks: data[1],
        })
    }
    
    /// Проверить совместимость с другим пиром
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        // Хотя бы один должен отправлять, а другой принимать
        let can_stream = (self.can_send && other.can_receive) 
                      || (self.can_receive && other.can_send);
        
        // Оба должны поддерживать Opus
        let codec_compatible = self.supports_opus && other.supports_opus;
        
        can_stream && codec_compatible
    }
}

/// Информация о треке для синхронизации
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// ID трека
    pub track_id: u8,
    /// Имя трека
    pub name: String,
    /// Битрейт в bps
    pub bitrate: u32,
    /// Количество каналов
    pub channels: u16,
    /// Включён FEC
    pub fec_enabled: bool,
}

impl TrackInfo {
    /// Сериализовать в байты
    pub fn serialize(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let name_len = name_bytes.len().min(255) as u8;
        
        let mut buf = Vec::with_capacity(8 + name_len as usize);
        buf.push(self.track_id);
        buf.extend_from_slice(&self.bitrate.to_le_bytes());
        buf.extend_from_slice(&self.channels.to_le_bytes());
        buf.push(if self.fec_enabled { 1 } else { 0 });
        buf.push(name_len);
        buf.extend_from_slice(&name_bytes[..name_len as usize]);
        
        buf
    }
    
    /// Десериализовать из байтов
    pub fn deserialize(data: &[u8]) -> Option<(Self, usize)> {
        if data.len() < 8 {
            return None;
        }
        
        let track_id = data[0];
        let bitrate = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let channels = u16::from_le_bytes([data[5], data[6]]);
        let fec_enabled = data[7] != 0;
        let name_len = data[8] as usize;
        
        if data.len() < 9 + name_len {
            return None;
        }
        
        let name = String::from_utf8_lossy(&data[9..9 + name_len]).to_string();
        
        Some((
            Self {
                track_id,
                name,
                bitrate,
                channels,
                fec_enabled,
            },
            9 + name_len,
        ))
    }
}

/// Пакет рукопожатия
#[derive(Debug, Clone)]
pub struct HandshakePacket {
    /// Тип пакета
    pub packet_type: HandshakePacketType,
    /// Уникальный ID сессии (для сопоставления запросов и ответов)
    pub session_id: u32,
    /// Данные пакета
    pub payload: Bytes,
}

impl HandshakePacket {
    /// Создать пакет Hello
    pub fn hello(session_id: u32, name: &str, audio_port: u16, capabilities: PeerCapabilities) -> Self {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(255) as u8;
        
        let mut payload = BytesMut::with_capacity(5 + name_len as usize);
        payload.put_slice(&audio_port.to_le_bytes());
        payload.put_slice(&capabilities.to_bytes());
        payload.put_u8(name_len);
        payload.put_slice(&name_bytes[..name_len as usize]);
        
        Self {
            packet_type: HandshakePacketType::Hello,
            session_id,
            payload: payload.freeze(),
        }
    }
    
    /// Разобрать данные Hello пакета
    pub fn parse_hello(&self) -> Option<(u16, PeerCapabilities, String)> {
        if self.payload.len() < 5 {
            return None;
        }
        
        let audio_port = u16::from_le_bytes([self.payload[0], self.payload[1]]);
        let capabilities = PeerCapabilities::from_bytes(&self.payload[2..4])?;
        let name_len = self.payload[4] as usize;
        
        if self.payload.len() < 5 + name_len {
            return None;
        }
        
        let name = String::from_utf8_lossy(&self.payload[5..5 + name_len]).to_string();
        
        Some((audio_port, capabilities, name))
    }
    
    /// Создать пакет HelloAck
    pub fn hello_ack(session_id: u32, name: &str, audio_port: u16, capabilities: PeerCapabilities) -> Self {
        let mut packet = Self::hello(session_id, name, audio_port, capabilities);
        packet.packet_type = HandshakePacketType::HelloAck;
        packet
    }
    
    /// Создать пакет SyncRequest
    pub fn sync_request(session_id: u32) -> Self {
        Self {
            packet_type: HandshakePacketType::SyncRequest,
            session_id,
            payload: Bytes::new(),
        }
    }
    
    /// Создать пакет SyncResponse с информацией о треках
    pub fn sync_response(session_id: u32, tracks: &[TrackInfo]) -> Self {
        let mut payload = BytesMut::new();
        payload.put_u8(tracks.len() as u8);
        
        for track in tracks {
            let track_data = track.serialize();
            payload.put_slice(&track_data);
        }
        
        Self {
            packet_type: HandshakePacketType::SyncResponse,
            session_id,
            payload: payload.freeze(),
        }
    }
    
    /// Разобрать данные SyncResponse
    pub fn parse_sync_response(&self) -> Option<Vec<TrackInfo>> {
        if self.payload.is_empty() {
            return Some(Vec::new());
        }
        
        let track_count = self.payload[0] as usize;
        let mut tracks = Vec::with_capacity(track_count);
        let mut offset = 1;
        
        for _ in 0..track_count {
            if offset >= self.payload.len() {
                break;
            }
            
            if let Some((track, consumed)) = TrackInfo::deserialize(&self.payload[offset..]) {
                tracks.push(track);
                offset += consumed;
            } else {
                break;
            }
        }
        
        Some(tracks)
    }
    
    /// Создать пакет Ping
    pub fn ping(session_id: u32) -> Self {
        Self {
            packet_type: HandshakePacketType::Ping,
            session_id,
            payload: Bytes::new(),
        }
    }
    
    /// Создать пакет Pong
    pub fn pong(session_id: u32) -> Self {
        Self {
            packet_type: HandshakePacketType::Pong,
            session_id,
            payload: Bytes::new(),
        }
    }
    
    /// Создать пакет Goodbye
    pub fn goodbye(session_id: u32) -> Self {
        Self {
            packet_type: HandshakePacketType::Goodbye,
            session_id,
            payload: Bytes::new(),
        }
    }
    
    /// Создать пакет Error
    pub fn error(session_id: u32, message: &str) -> Self {
        let msg_bytes = message.as_bytes();
        let msg_len = msg_bytes.len().min(255);
        
        let mut payload = BytesMut::with_capacity(1 + msg_len);
        payload.put_u8(msg_len as u8);
        payload.put_slice(&msg_bytes[..msg_len]);
        
        Self {
            packet_type: HandshakePacketType::ErrorPacket,
            session_id,
            payload: payload.freeze(),
        }
    }
    
    /// Разобрать сообщение об ошибке
    pub fn parse_error(&self) -> Option<String> {
        if self.payload.is_empty() {
            return Some(String::new());
        }
        
        let msg_len = self.payload[0] as usize;
        if self.payload.len() < 1 + msg_len {
            return None;
        }
        
        Some(String::from_utf8_lossy(&self.payload[1..1 + msg_len]).to_string())
    }
    
    /// Сериализовать пакет
    pub fn serialize(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(10 + self.payload.len());
        
        // Магические байты
        buf.put_slice(HANDSHAKE_MAGIC);
        // Версия протокола
        buf.put_u8(PROTOCOL_VERSION);
        // Тип пакета
        buf.put_u8(self.packet_type as u8);
        // ID сессии
        buf.put_u32_le(self.session_id);
        // Payload
        buf.put_slice(&self.payload);
        
        buf.freeze()
    }
    
    /// Десериализовать пакет
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 10 {
            return None;
        }
        
        // Проверяем магические байты
        if &data[0..4] != HANDSHAKE_MAGIC {
            return None;
        }
        
        // Проверяем версию
        let version = data[4];
        if version != PROTOCOL_VERSION {
            return None;
        }
        
        let packet_type = HandshakePacketType::try_from(data[5]).ok()?;
        let session_id = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
        let payload = Bytes::copy_from_slice(&data[10..]);
        
        Some(Self {
            packet_type,
            session_id,
            payload,
        })
    }
}

/// Состояние рукопожатия с пиром
#[derive(Debug, Clone)]
pub enum HandshakeState {
    /// Ожидание начала
    Idle,
    /// Отправлен Hello, ждём HelloAck
    HelloSent { sent_at: Instant },
    /// Получен Hello, отправлен HelloAck
    HelloReceived { peer_caps: PeerCapabilities },
    /// Рукопожатие завершено успешно
    Connected {
        peer_name: String,
        peer_caps: PeerCapabilities,
        audio_port: u16,
        connected_at: Instant,
    },
    /// Ошибка рукопожатия
    Failed { reason: String },
}

/// Менеджер рукопожатия с пирами
pub struct HandshakeManager {
    /// Наше имя
    our_name: String,
    /// Наш аудио порт
    our_audio_port: u16,
    /// Наши возможности
    our_capabilities: PeerCapabilities,
    /// Состояния рукопожатия с пирами
    states: parking_lot::RwLock<HashMap<SocketAddr, HandshakeState>>,
    /// ID сессии (инкрементируется для каждого нового рукопожатия)
    next_session_id: std::sync::atomic::AtomicU32,
}

impl HandshakeManager {
    /// Создать новый менеджер
    pub fn new(name: String, audio_port: u16, capabilities: PeerCapabilities) -> Self {
        Self {
            our_name: name,
            our_audio_port: audio_port,
            our_capabilities: capabilities,
            states: parking_lot::RwLock::new(HashMap::new()),
            next_session_id: std::sync::atomic::AtomicU32::new(1),
        }
    }
    
    /// Получить новый ID сессии
    fn new_session_id(&self) -> u32 {
        self.next_session_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
    
    /// Инициировать рукопожатие с пиром
    pub fn initiate(&self, peer_addr: SocketAddr) -> HandshakePacket {
        let session_id = self.new_session_id();
        
        self.states.write().insert(
            peer_addr,
            HandshakeState::HelloSent { sent_at: Instant::now() },
        );
        
        HandshakePacket::hello(
            session_id,
            &self.our_name,
            self.our_audio_port,
            self.our_capabilities,
        )
    }
    
    /// Обработать входящий пакет рукопожатия
    pub fn process_packet(
        &self,
        peer_addr: SocketAddr,
        packet: HandshakePacket,
    ) -> Option<HandshakePacket> {
        match packet.packet_type {
            HandshakePacketType::Hello => {
                // Получили приветствие - отвечаем HelloAck
                if let Some((audio_port, peer_caps, peer_name)) = packet.parse_hello() {
                    // Проверяем совместимость
                    if !self.our_capabilities.is_compatible_with(&peer_caps) {
                        return Some(HandshakePacket::error(
                            packet.session_id,
                            "Несовместимые возможности пиров",
                        ));
                    }
                    
                    // Обновляем состояние
                    self.states.write().insert(
                        peer_addr,
                        HandshakeState::Connected {
                            peer_name,
                            peer_caps,
                            audio_port,
                            connected_at: Instant::now(),
                        },
                    );
                    
                    // Отвечаем HelloAck
                    return Some(HandshakePacket::hello_ack(
                        packet.session_id,
                        &self.our_name,
                        self.our_audio_port,
                        self.our_capabilities,
                    ));
                }
            }
            
            HandshakePacketType::HelloAck => {
                // Получили подтверждение - рукопожатие завершено
                if let Some((audio_port, peer_caps, peer_name)) = packet.parse_hello() {
                    self.states.write().insert(
                        peer_addr,
                        HandshakeState::Connected {
                            peer_name,
                            peer_caps,
                            audio_port,
                            connected_at: Instant::now(),
                        },
                    );
                }
            }
            
            HandshakePacketType::Ping => {
                // Отвечаем на пинг
                return Some(HandshakePacket::pong(packet.session_id));
            }
            
            HandshakePacketType::Goodbye => {
                // Пир отключается
                self.states.write().remove(&peer_addr);
            }
            
            HandshakePacketType::ErrorPacket => {
                // Получили ошибку
                let reason = packet.parse_error().unwrap_or_default();
                self.states.write().insert(
                    peer_addr,
                    HandshakeState::Failed { reason },
                );
            }
            
            _ => {}
        }
        
        None
    }
    
    /// Получить состояние рукопожатия с пиром
    pub fn get_state(&self, peer_addr: &SocketAddr) -> Option<HandshakeState> {
        self.states.read().get(peer_addr).cloned()
    }
    
    /// Проверить, подключён ли пир
    pub fn is_connected(&self, peer_addr: &SocketAddr) -> bool {
        matches!(
            self.states.read().get(peer_addr),
            Some(HandshakeState::Connected { .. })
        )
    }
    
    /// Получить список подключённых пиров
    pub fn connected_peers(&self) -> Vec<(SocketAddr, String, u16)> {
        self.states
            .read()
            .iter()
            .filter_map(|(addr, state)| {
                if let HandshakeState::Connected { peer_name, audio_port, .. } = state {
                    Some((*addr, peer_name.clone(), *audio_port))
                } else {
                    None
                }
            })
            .collect()
    }
    
    /// Очистить устаревшие состояния
    pub fn cleanup_stale(&self, timeout: Duration) {
        let mut states = self.states.write();
        states.retain(|_, state| {
            match state {
                HandshakeState::HelloSent { sent_at } => {
                    sent_at.elapsed() < timeout
                }
                HandshakeState::Connected { connected_at: _, .. } => {
                    // Подключённые пиры не удаляем по таймауту
                    true
                }
                HandshakeState::Failed { .. } => {
                    // Удаляем неудачные через минуту
                    false
                }
                _ => true,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_capabilities_serialization() {
        let caps = PeerCapabilities::full();
        let bytes = caps.to_bytes();
        let restored = PeerCapabilities::from_bytes(&bytes).unwrap();
        
        assert_eq!(caps.can_send, restored.can_send);
        assert_eq!(caps.can_receive, restored.can_receive);
        assert_eq!(caps.supports_opus, restored.supports_opus);
        assert_eq!(caps.max_tracks, restored.max_tracks);
    }
    
    #[test]
    fn test_hello_packet() {
        let packet = HandshakePacket::hello(
            12345,
            "Test Peer",
            5000,
            PeerCapabilities::full(),
        );
        
        let serialized = packet.serialize();
        let deserialized = HandshakePacket::deserialize(&serialized).unwrap();
        
        assert_eq!(deserialized.packet_type, HandshakePacketType::Hello);
        assert_eq!(deserialized.session_id, 12345);
        
        let (port, caps, name) = deserialized.parse_hello().unwrap();
        assert_eq!(port, 5000);
        assert_eq!(name, "Test Peer");
        assert!(caps.can_send);
        assert!(caps.can_receive);
    }
    
    #[test]
    fn test_track_info_serialization() {
        let track = TrackInfo {
            track_id: 5,
            name: "Микрофон".to_string(),
            bitrate: 128000,
            channels: 2,
            fec_enabled: true,
        };
        
        let bytes = track.serialize();
        let (restored, _) = TrackInfo::deserialize(&bytes).unwrap();
        
        assert_eq!(track.track_id, restored.track_id);
        assert_eq!(track.name, restored.name);
        assert_eq!(track.bitrate, restored.bitrate);
        assert_eq!(track.channels, restored.channels);
        assert_eq!(track.fec_enabled, restored.fec_enabled);
    }
    
    #[test]
    fn test_capabilities_compatibility() {
        let sender = PeerCapabilities::sender_only();
        let receiver = PeerCapabilities::receiver_only();
        let full = PeerCapabilities::full();
        
        // Отправитель совместим с получателем
        assert!(sender.is_compatible_with(&receiver));
        assert!(receiver.is_compatible_with(&sender));
        
        // Полные возможности совместимы со всеми
        assert!(full.is_compatible_with(&sender));
        assert!(full.is_compatible_with(&receiver));
        assert!(full.is_compatible_with(&full));
        
        // Два отправителя несовместимы
        assert!(!sender.is_compatible_with(&sender));
        
        // Два получателя несовместимы
        assert!(!receiver.is_compatible_with(&receiver));
    }
}
