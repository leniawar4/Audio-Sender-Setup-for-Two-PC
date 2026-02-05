//! Network subsystem for UDP audio transport

pub mod udp;
pub mod sender;
pub mod receiver;
pub mod discovery;

pub use udp::{UdpSocket, create_socket};
pub use sender::AudioSender;
pub use receiver::AudioReceiver;
pub use discovery::{DiscoveryService, DiscoveredPeer, get_local_addresses, get_best_local_address};
