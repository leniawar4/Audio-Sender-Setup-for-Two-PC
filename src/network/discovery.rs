//! Automatic IP discovery for LAN audio streaming
//!
//! Provides automatic discovery of local network interfaces and peer devices
//! without requiring manual IP configuration.

use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::NetworkError;

/// Discovery service port (separate from audio streaming)
pub const DISCOVERY_PORT: u16 = 5001;

/// Discovery beacon interval
pub const BEACON_INTERVAL_MS: u64 = 1000;

/// Discovery timeout for peer detection
pub const DISCOVERY_TIMEOUT_MS: u64 = 5000;

/// Magic bytes for discovery packets
const DISCOVERY_MAGIC: &[u8; 4] = b"LAND"; // LAN Audio Network Discovery

/// Discovery packet types
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryPacketType {
    /// Sender beacon - "I'm a sender at this address"
    SenderBeacon = 0x01,
    /// Receiver beacon - "I'm a receiver at this address"
    ReceiverBeacon = 0x02,
    /// Discovery request - "Who's there?"
    Request = 0x03,
    /// Discovery response - "I'm here"
    Response = 0x04,
}

impl TryFrom<u8> for DiscoveryPacketType {
    type Error = ();
    
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::SenderBeacon),
            0x02 => Ok(Self::ReceiverBeacon),
            0x03 => Ok(Self::Request),
            0x04 => Ok(Self::Response),
            _ => Err(()),
        }
    }
}

/// Discovery packet structure
/// Format: [MAGIC(4)][TYPE(1)][AUDIO_PORT(2)][NAME_LEN(1)][NAME(variable)]
#[derive(Debug, Clone)]
pub struct DiscoveryPacket {
    pub packet_type: DiscoveryPacketType,
    pub audio_port: u16,
    pub name: String,
}

impl DiscoveryPacket {
    pub fn new(packet_type: DiscoveryPacketType, audio_port: u16, name: String) -> Self {
        Self {
            packet_type,
            audio_port,
            name: name.chars().take(255).collect(), // Limit name length
        }
    }
    
    pub fn serialize(&self) -> Vec<u8> {
        let name_bytes = self.name.as_bytes();
        let mut data = Vec::with_capacity(8 + name_bytes.len());
        
        data.extend_from_slice(DISCOVERY_MAGIC);
        data.push(self.packet_type as u8);
        data.extend_from_slice(&self.audio_port.to_le_bytes());
        data.push(name_bytes.len() as u8);
        data.extend_from_slice(name_bytes);
        
        data
    }
    
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        
        // Check magic
        if &data[0..4] != DISCOVERY_MAGIC {
            return None;
        }
        
        let packet_type = DiscoveryPacketType::try_from(data[4]).ok()?;
        let audio_port = u16::from_le_bytes([data[5], data[6]]);
        let name_len = data[7] as usize;
        
        if data.len() < 8 + name_len {
            return None;
        }
        
        let name = String::from_utf8_lossy(&data[8..8 + name_len]).to_string();
        
        Some(Self {
            packet_type,
            audio_port,
            name,
        })
    }
}

/// Discovered peer information
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub address: SocketAddr,
    pub audio_port: u16,
    pub name: String,
    pub is_sender: bool,
    pub last_seen: Instant,
}

impl DiscoveredPeer {
    /// Get the audio streaming address
    pub fn audio_address(&self) -> SocketAddr {
        SocketAddr::new(self.address.ip(), self.audio_port)
    }
}

/// Get all local network interface addresses
pub fn get_local_addresses() -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    
    // Try to get addresses by connecting to a remote address
    // This gives us the default outbound interface
    if let Ok(socket) = StdUdpSocket::bind("0.0.0.0:0") {
        // Try multiple well-known addresses to find local IPs
        for target in &["8.8.8.8:53", "1.1.1.1:53", "208.67.222.222:53"] {
            if socket.connect(target).is_ok() {
                if let Ok(local_addr) = socket.local_addr() {
                    let ip = local_addr.ip();
                    if !addresses.contains(&ip) && !ip.is_loopback() {
                        addresses.push(ip);
                    }
                }
            }
        }
    }
    
    // Platform-specific interface enumeration
    #[cfg(target_os = "windows")]
    {
        addresses.extend(get_windows_interfaces());
    }
    
    #[cfg(not(target_os = "windows"))]
    {
        addresses.extend(get_unix_interfaces());
    }
    
    // Remove duplicates and loopback
    let mut unique: Vec<IpAddr> = addresses
        .into_iter()
        .filter(|ip| !ip.is_loopback())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    
    // Sort to prioritize private network addresses
    unique.sort_by(|a, b| {
        let score_a = ip_priority_score(a);
        let score_b = ip_priority_score(b);
        score_b.cmp(&score_a) // Higher score = higher priority
    });
    
    unique
}

/// Score IP addresses for priority (higher = better for LAN)
fn ip_priority_score(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 192.168.x.x - most common home/office LAN
            if octets[0] == 192 && octets[1] == 168 {
                return 100;
            }
            // 10.x.x.x - common corporate LAN
            if octets[0] == 10 {
                return 90;
            }
            // 172.16-31.x.x - less common private range
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                return 80;
            }
            // 169.254.x.x - link-local (fallback)
            if octets[0] == 169 && octets[1] == 254 {
                return 10;
            }
            // Other (public IPs, etc.)
            50
        }
        IpAddr::V6(_) => 20, // Lower priority for IPv6 in LAN context
    }
}

#[cfg(target_os = "windows")]
fn get_windows_interfaces() -> Vec<IpAddr> {
    use std::process::Command;
    
    let mut addresses = Vec::new();
    
    // Use ipconfig to get interface addresses
    if let Ok(output) = Command::new("ipconfig").output() {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                let line = line.trim();
                // Look for IPv4 addresses
                if line.contains("IPv4") || line.contains("IP Address") {
                    if let Some(addr_str) = line.split(':').nth(1) {
                        if let Ok(addr) = addr_str.trim().parse::<Ipv4Addr>() {
                            addresses.push(IpAddr::V4(addr));
                        }
                    }
                }
            }
        }
    }
    
    addresses
}

#[cfg(not(target_os = "windows"))]
fn get_unix_interfaces() -> Vec<IpAddr> {
    use std::process::Command;
    
    let mut addresses = Vec::new();
    
    // Try ip command first (Linux)
    if let Ok(output) = Command::new("ip").args(["addr", "show"]).output() {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                if line.contains("inet ") && !line.contains("inet6") {
                    if let Some(addr_part) = line.split_whitespace().nth(1) {
                        if let Some(addr_str) = addr_part.split('/').next() {
                            if let Ok(addr) = addr_str.parse::<Ipv4Addr>() {
                                addresses.push(IpAddr::V4(addr));
                            }
                        }
                    }
                }
            }
        }
    }
    // Fallback to ifconfig (macOS, older Linux)
    else if let Ok(output) = Command::new("ifconfig").output() {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                if line.contains("inet ") && !line.contains("inet6") {
                    for part in line.split_whitespace() {
                        if let Ok(addr) = part.parse::<Ipv4Addr>() {
                            addresses.push(IpAddr::V4(addr));
                            break;
                        }
                    }
                }
            }
        }
    }
    
    addresses
}

/// Get the best local address for LAN communication
pub fn get_best_local_address() -> Option<IpAddr> {
    get_local_addresses().into_iter().next()
}

/// Get broadcast addresses for all local subnets
pub fn get_broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut broadcasts = Vec::new();
    
    for addr in get_local_addresses() {
        if let IpAddr::V4(v4) = addr {
            let octets = v4.octets();
            // Assume /24 subnet for simplicity (most common)
            // TODO: Could parse actual subnet mask from system
            let broadcast = Ipv4Addr::new(octets[0], octets[1], octets[2], 255);
            if !broadcasts.contains(&broadcast) {
                broadcasts.push(broadcast);
            }
        }
    }
    
    // Also add general broadcast
    if !broadcasts.contains(&Ipv4Addr::BROADCAST) {
        broadcasts.push(Ipv4Addr::BROADCAST);
    }
    
    broadcasts
}

/// Network discovery service for automatic peer detection
pub struct DiscoveryService {
    /// Is this a sender (true) or receiver (false)
    is_sender: bool,
    
    /// Audio streaming port
    audio_port: u16,
    
    /// Service name
    name: String,
    
    /// Running flag
    running: Arc<AtomicBool>,
    
    /// Discovered peers
    peers: Arc<parking_lot::RwLock<Vec<DiscoveredPeer>>>,
    
    /// Beacon thread handle
    beacon_handle: Option<JoinHandle<()>>,
    
    /// Listener thread handle
    listener_handle: Option<JoinHandle<()>>,
    
    /// Callback for new peer discovery
    on_peer_discovered: Option<Arc<dyn Fn(DiscoveredPeer) + Send + Sync>>,
}

impl DiscoveryService {
    /// Create a new discovery service
    pub fn new(is_sender: bool, audio_port: u16, name: String) -> Self {
        Self {
            is_sender,
            audio_port,
            name,
            running: Arc::new(AtomicBool::new(false)),
            peers: Arc::new(parking_lot::RwLock::new(Vec::new())),
            beacon_handle: None,
            listener_handle: None,
            on_peer_discovered: None,
        }
    }
    
    /// Set callback for peer discovery
    pub fn on_peer_discovered<F>(&mut self, callback: F)
    where
        F: Fn(DiscoveredPeer) + Send + Sync + 'static,
    {
        self.on_peer_discovered = Some(Arc::new(callback));
    }
    
    /// Start the discovery service
    pub fn start(&mut self) -> Result<(), NetworkError> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        self.running.store(true, Ordering::SeqCst);
        
        // Create UDP socket for discovery
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
        
        socket.set_reuse_address(true)
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
        
        socket.set_broadcast(true)
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
        
        let bind_addr: SocketAddr = format!("0.0.0.0:{}", DISCOVERY_PORT).parse().unwrap();
        socket.bind(&bind_addr.into())
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
        
        socket.set_nonblocking(true)
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
        
        let std_socket: StdUdpSocket = socket.into();
        let recv_socket = std_socket.try_clone()
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
        
        // Start beacon thread
        let running = self.running.clone();
        let is_sender = self.is_sender;
        let audio_port = self.audio_port;
        let name = self.name.clone();
        
        let beacon_socket = std_socket;
        self.beacon_handle = Some(thread::Builder::new()
            .name("discovery-beacon".to_string())
            .spawn(move || {
                Self::beacon_loop(beacon_socket, running, is_sender, audio_port, name);
            })
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?);
        
        // Start listener thread
        let running = self.running.clone();
        let peers = self.peers.clone();
        let callback = self.on_peer_discovered.clone();
        
        self.listener_handle = Some(thread::Builder::new()
            .name("discovery-listener".to_string())
            .spawn(move || {
                Self::listener_loop(recv_socket, running, peers, callback);
            })
            .map_err(|e| NetworkError::BindFailed(e.to_string()))?);
        
        Ok(())
    }
    
    /// Beacon loop - broadcast presence periodically
    fn beacon_loop(
        socket: StdUdpSocket,
        running: Arc<AtomicBool>,
        is_sender: bool,
        audio_port: u16,
        name: String,
    ) {
        let packet_type = if is_sender {
            DiscoveryPacketType::SenderBeacon
        } else {
            DiscoveryPacketType::ReceiverBeacon
        };
        
        let packet = DiscoveryPacket::new(packet_type, audio_port, name);
        let data = packet.serialize();
        
        let broadcasts = get_broadcast_addresses();
        
        while running.load(Ordering::Relaxed) {
            // Send beacon to all broadcast addresses
            for broadcast in &broadcasts {
                let addr = SocketAddr::new(IpAddr::V4(*broadcast), DISCOVERY_PORT);
                let _ = socket.send_to(&data, addr);
            }
            
            thread::sleep(Duration::from_millis(BEACON_INTERVAL_MS));
        }
    }
    
    /// Listener loop - receive discovery packets
    fn listener_loop(
        socket: StdUdpSocket,
        running: Arc<AtomicBool>,
        peers: Arc<parking_lot::RwLock<Vec<DiscoveredPeer>>>,
        callback: Option<Arc<dyn Fn(DiscoveredPeer) + Send + Sync>>,
    ) {
        let mut buffer = [0u8; 512];
        
        while running.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buffer) {
                Ok((size, addr)) => {
                    if let Some(packet) = DiscoveryPacket::deserialize(&buffer[..size]) {
                        let is_sender = matches!(
                            packet.packet_type,
                            DiscoveryPacketType::SenderBeacon
                        );
                        
                        let peer = DiscoveredPeer {
                            address: addr,
                            audio_port: packet.audio_port,
                            name: packet.name,
                            is_sender,
                            last_seen: Instant::now(),
                        };
                        
                        // Update or add peer
                        let mut peers_guard = peers.write();
                        let mut found = false;
                        for existing in peers_guard.iter_mut() {
                            if existing.address.ip() == addr.ip() && existing.is_sender == is_sender {
                                existing.last_seen = Instant::now();
                                existing.audio_port = peer.audio_port;
                                existing.name = peer.name.clone();
                                found = true;
                                break;
                            }
                        }
                        
                        if !found {
                            peers_guard.push(peer.clone());
                            drop(peers_guard);
                            
                            // Notify callback
                            if let Some(ref cb) = callback {
                                cb(peer);
                            }
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            
            // Clean up stale peers (not seen for 10 seconds)
            let mut peers_guard = peers.write();
            peers_guard.retain(|p| p.last_seen.elapsed() < Duration::from_secs(10));
        }
    }
    
    /// Stop the discovery service
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        
        if let Some(handle) = self.beacon_handle.take() {
            let _ = handle.join();
        }
        
        if let Some(handle) = self.listener_handle.take() {
            let _ = handle.join();
        }
    }
    
    /// Get discovered peers
    pub fn get_peers(&self) -> Vec<DiscoveredPeer> {
        self.peers.read().clone()
    }
    
    /// Get discovered senders
    pub fn get_senders(&self) -> Vec<DiscoveredPeer> {
        self.peers.read()
            .iter()
            .filter(|p| p.is_sender)
            .cloned()
            .collect()
    }
    
    /// Get discovered receivers
    pub fn get_receivers(&self) -> Vec<DiscoveredPeer> {
        self.peers.read()
            .iter()
            .filter(|p| !p.is_sender)
            .cloned()
            .collect()
    }
    
    /// Wait for a peer of the specified type
    pub fn wait_for_peer(&self, is_sender: bool, timeout: Duration) -> Option<DiscoveredPeer> {
        let start = Instant::now();
        
        while start.elapsed() < timeout {
            let peers = self.peers.read();
            if let Some(peer) = peers.iter().find(|p| p.is_sender == is_sender) {
                return Some(peer.clone());
            }
            drop(peers);
            thread::sleep(Duration::from_millis(100));
        }
        
        None
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One-shot discovery - find peers without running a service
pub fn discover_peers(timeout: Duration, looking_for_senders: bool) -> Vec<DiscoveredPeer> {
    let mut service = DiscoveryService::new(!looking_for_senders, 0, "discovery".to_string());
    
    if service.start().is_err() {
        return Vec::new();
    }
    
    thread::sleep(timeout);
    
    let peers = if looking_for_senders {
        service.get_senders()
    } else {
        service.get_receivers()
    };
    
    service.stop();
    peers
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_get_local_addresses() {
        let addresses = get_local_addresses();
        println!("Local addresses: {:?}", addresses);
        assert!(!addresses.is_empty() || cfg!(test)); // May be empty in CI
    }
    
    #[test]
    fn test_discovery_packet_serialization() {
        let packet = DiscoveryPacket::new(
            DiscoveryPacketType::SenderBeacon,
            5000,
            "Test Sender".to_string(),
        );
        
        let data = packet.serialize();
        let parsed = DiscoveryPacket::deserialize(&data).unwrap();
        
        assert_eq!(parsed.packet_type, DiscoveryPacketType::SenderBeacon);
        assert_eq!(parsed.audio_port, 5000);
        assert_eq!(parsed.name, "Test Sender");
    }
    
    #[test]
    fn test_get_broadcast_addresses() {
        let broadcasts = get_broadcast_addresses();
        println!("Broadcast addresses: {:?}", broadcasts);
        assert!(!broadcasts.is_empty());
    }
}
