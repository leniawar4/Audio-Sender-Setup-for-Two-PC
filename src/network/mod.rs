//! Сетевая подсистема для UDP транспорта аудио
//!
//! Содержит модули для:
//! - UDP сокетов и передачи пакетов
//! - Отправки и приёма аудио
//! - Автоматического обнаружения пиров
//! - Протокола рукопожатия для синхронизации

pub mod udp;
pub mod sender;
pub mod receiver;
pub mod discovery;
pub mod handshake;

pub use udp::{UdpSocket, create_socket};
pub use sender::AudioSender;
pub use receiver::AudioReceiver;
pub use discovery::{DiscoveryService, DiscoveredPeer, get_local_addresses, get_best_local_address};
pub use handshake::{HandshakeManager, HandshakePacket, PeerCapabilities, HandshakeState};
