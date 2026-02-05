//! Low-level UDP socket handling
//!
//! Optimized for low-latency audio streaming with configurable
//! buffer sizes and non-blocking I/O.

use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::io;
use tokio::net::UdpSocket as TokioUdpSocket;

use crate::config::NetworkConfig;
use crate::error::NetworkError;

/// Re-export for convenience
pub type UdpSocket = TokioUdpSocket;

/// Create a configured UDP socket for audio streaming
pub fn create_socket(config: &NetworkConfig) -> Result<StdUdpSocket, NetworkError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
    
    // Set socket options for low latency
    configure_socket(&socket, config)?;
    
    // Bind to address
    let addr: SocketAddr = format!("{}:{}", config.bind_address, config.udp_port)
        .parse()
        .map_err(|e: std::net::AddrParseError| NetworkError::BindFailed(e.to_string()))?;
    
    socket.bind(&addr.into())
        .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
    
    // Convert to std socket
    let std_socket: StdUdpSocket = socket.into();
    std_socket.set_nonblocking(true)
        .map_err(|e| NetworkError::BindFailed(e.to_string()))?;
    
    Ok(std_socket)
}

/// Create async UDP socket for tokio
pub async fn create_async_socket(config: &NetworkConfig) -> Result<TokioUdpSocket, NetworkError> {
    let std_socket = create_socket(config)?;
    TokioUdpSocket::from_std(std_socket)
        .map_err(|e| NetworkError::BindFailed(e.to_string()))
}

/// Configure socket options for low-latency audio
fn configure_socket(socket: &Socket, config: &NetworkConfig) -> Result<(), NetworkError> {
    // Allow address reuse
    if config.reuse_addr {
        socket.set_reuse_address(true)
            .map_err(|e| NetworkError::BindFailed(format!("Failed to set SO_REUSEADDR: {}", e)))?;
    }
    
    // Set send buffer size - larger buffers prevent packet loss under load
    socket.set_send_buffer_size(config.send_buffer_size)
        .map_err(|e| NetworkError::BindFailed(format!("Failed to set send buffer: {}", e)))?;
    
    // Set receive buffer size - larger buffers handle burst traffic better
    socket.set_recv_buffer_size(config.recv_buffer_size)
        .map_err(|e| NetworkError::BindFailed(format!("Failed to set recv buffer: {}", e)))?;
    
    // Enable broadcast (useful for local network discovery and fallback)
    socket.set_broadcast(true)
        .map_err(|e| NetworkError::BindFailed(format!("Failed to set broadcast: {}", e)))?;
    
    // Platform-specific optimizations
    #[cfg(target_os = "linux")]
    {
        configure_linux_socket(socket)?;
    }
    
    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_linux_socket(_socket: &Socket) -> Result<(), NetworkError> {
    // Note: On Linux, setting IP_TOS and SO_BUSY_POLL would require libc
    // These optimizations are available but require adding libc dependency
    // For now, the socket2 configuration handles the most important settings
    
    // If libc is added to Cargo.toml, the following could be enabled:
    // - IP_TOS for DSCP marking (QoS)
    // - SO_BUSY_POLL for reduced latency polling
    
    Ok(())
}

/// High-performance packet sender
pub struct PacketSender {
    socket: StdUdpSocket,
    target: SocketAddr,
    packets_sent: std::sync::atomic::AtomicU64,
    bytes_sent: std::sync::atomic::AtomicU64,
}

impl PacketSender {
    pub fn new(socket: StdUdpSocket, target: SocketAddr) -> Self {
        Self {
            socket,
            target,
            packets_sent: std::sync::atomic::AtomicU64::new(0),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
        }
    }
    
    /// Send packet to target
    pub fn send(&self, data: &[u8]) -> io::Result<usize> {
        let sent = self.socket.send_to(data, self.target)?;
        self.packets_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes_sent.fetch_add(sent as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(sent)
    }
    
    /// Get packets sent count
    pub fn packets_sent(&self) -> u64 {
        self.packets_sent.load(std::sync::atomic::Ordering::Relaxed)
    }
    
    /// Get bytes sent count
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(std::sync::atomic::Ordering::Relaxed)
    }
    
    /// Update target address
    pub fn set_target(&mut self, target: SocketAddr) {
        self.target = target;
    }
}

/// High-performance packet receiver
pub struct PacketReceiver {
    socket: StdUdpSocket,
    recv_buffer: Vec<u8>,
    packets_received: std::sync::atomic::AtomicU64,
    bytes_received: std::sync::atomic::AtomicU64,
}

impl PacketReceiver {
    pub fn new(socket: StdUdpSocket, buffer_size: usize) -> Self {
        Self {
            socket,
            recv_buffer: vec![0u8; buffer_size],
            packets_received: std::sync::atomic::AtomicU64::new(0),
            bytes_received: std::sync::atomic::AtomicU64::new(0),
        }
    }
    
    /// Receive packet (blocking)
    pub fn recv(&mut self) -> io::Result<(&[u8], SocketAddr)> {
        let (size, addr) = self.socket.recv_from(&mut self.recv_buffer)?;
        self.packets_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes_received.fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
        Ok((&self.recv_buffer[..size], addr))
    }
    
    /// Try to receive packet (non-blocking)
    pub fn try_recv(&mut self) -> io::Result<Option<(&[u8], SocketAddr)>> {
        match self.socket.recv_from(&mut self.recv_buffer) {
            Ok((size, addr)) => {
                self.packets_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.bytes_received.fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
                Ok(Some((&self.recv_buffer[..size], addr)))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
    
    /// Get packets received count
    pub fn packets_received(&self) -> u64 {
        self.packets_received.load(std::sync::atomic::Ordering::Relaxed)
    }
    
    /// Get bytes received count
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Network statistics
#[derive(Debug, Clone, Default)]
pub struct NetworkStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_lost: u64,
    pub rtt_ms: f32,
    pub jitter_ms: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_socket_creation() {
        let config = NetworkConfig {
            bind_address: "127.0.0.1".to_string(),
            udp_port: 0, // Let OS assign port
            ..Default::default()
        };
        
        let socket = create_socket(&config);
        assert!(socket.is_ok());
    }
}
