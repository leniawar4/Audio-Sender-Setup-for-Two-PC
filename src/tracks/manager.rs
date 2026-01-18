//! Track manager for handling multiple audio tracks

use dashmap::DashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::broadcast;

use crate::error::TrackError;
use crate::protocol::{TrackConfig, TrackConfigUpdate, TrackStatus};
use crate::tracks::track::Track;
use crate::constants::MAX_TRACKS;

/// Events emitted by the track manager
#[derive(Debug, Clone)]
pub enum TrackEvent {
    Created(u8),
    Removed(u8),
    Started(u8),
    Stopped(u8),
    ConfigUpdated(u8),
    /// Device changed event: (track_id, old_device_id, new_device_id)
    DeviceChanged(u8, String, String),
    Error(u8, String),
}

/// Track manager for sender or receiver
pub struct TrackManager {
    /// All tracks indexed by ID
    tracks: DashMap<u8, Track>,
    
    /// Next available track ID
    next_id: AtomicU8,
    
    /// Event broadcaster
    event_tx: broadcast::Sender<TrackEvent>,
    
    /// Event receiver (for cloning)
    _event_rx: broadcast::Receiver<TrackEvent>,
    
    /// Maximum tracks allowed
    max_tracks: usize,
    
    /// Solo mode active (any track soloed)
    solo_active: std::sync::atomic::AtomicBool,
}

impl TrackManager {
    /// Create a new track manager
    pub fn new() -> Self {
        let (event_tx, event_rx) = broadcast::channel(256);
        
        Self {
            tracks: DashMap::new(),
            next_id: AtomicU8::new(0),
            event_tx,
            _event_rx: event_rx,
            max_tracks: MAX_TRACKS,
            solo_active: std::sync::atomic::AtomicBool::new(false),
        }
    }
    
    /// Subscribe to track events
    pub fn subscribe(&self) -> broadcast::Receiver<TrackEvent> {
        self.event_tx.subscribe()
    }
    
    /// Create a new track
    pub fn create_track(&self, mut config: TrackConfig) -> Result<u8, TrackError> {
        if self.tracks.len() >= self.max_tracks {
            return Err(TrackError::MaxTracksReached(self.max_tracks));
        }
        
        // Assign ID if not provided
        let id = config.track_id.unwrap_or_else(|| {
            self.next_id.fetch_add(1, Ordering::SeqCst)
        });
        
        // Check if ID already exists
        if self.tracks.contains_key(&id) {
            return Err(TrackError::AlreadyExists(id));
        }
        
        config.track_id = Some(id);
        let track = Track::new(id, config);
        
        self.tracks.insert(id, track);
        let _ = self.event_tx.send(TrackEvent::Created(id));
        
        Ok(id)
    }
    
    /// Remove a track
    pub fn remove_track(&self, track_id: u8) -> Result<Track, TrackError> {
        let (_, mut track) = self.tracks
            .remove(&track_id)
            .ok_or(TrackError::NotFound(track_id))?;
        
        // Stop track if running
        track.stop();
        
        let _ = self.event_tx.send(TrackEvent::Removed(track_id));
        
        // Update solo state
        self.update_solo_state();
        
        Ok(track)
    }
    
    /// Get track reference
    pub fn get_track(&self, track_id: u8) -> Option<dashmap::mapref::one::Ref<'_, u8, Track>> {
        self.tracks.get(&track_id)
    }
    
    /// Get mutable track reference
    pub fn get_track_mut(&self, track_id: u8) -> Option<dashmap::mapref::one::RefMut<'_, u8, Track>> {
        self.tracks.get_mut(&track_id)
    }
    
    /// Start a track
    pub fn start_track(&self, track_id: u8) -> Result<(), TrackError> {
        let mut track = self.tracks
            .get_mut(&track_id)
            .ok_or(TrackError::NotFound(track_id))?;
        
        track.start()?;
        let _ = self.event_tx.send(TrackEvent::Started(track_id));
        
        Ok(())
    }
    
    /// Stop a track
    pub fn stop_track(&self, track_id: u8) -> Result<(), TrackError> {
        let mut track = self.tracks
            .get_mut(&track_id)
            .ok_or(TrackError::NotFound(track_id))?;
        
        track.stop();
        let _ = self.event_tx.send(TrackEvent::Stopped(track_id));
        
        Ok(())
    }
    
    /// Start all tracks
    pub fn start_all(&self) -> Vec<Result<(), TrackError>> {
        self.tracks
            .iter()
            .map(|entry| {
                let id = *entry.key();
                self.start_track(id)
            })
            .collect()
    }
    
    /// Stop all tracks
    pub fn stop_all(&self) {
        for mut entry in self.tracks.iter_mut() {
            entry.stop();
            let _ = self.event_tx.send(TrackEvent::Stopped(*entry.key()));
        }
    }
    
    /// Update track configuration
    pub fn update_track(&self, track_id: u8, update: TrackConfigUpdate) -> Result<(), TrackError> {
        let mut track = self.tracks
            .get_mut(&track_id)
            .ok_or(TrackError::NotFound(track_id))?;
        
        // Check if device_id is changing
        let old_device_id = track.device_id.clone();
        let new_device_id = update.device_id.clone();
        
        track.update_config(&update)?;
        
        // Emit DeviceChanged event if device changed
        if let Some(ref new_id) = new_device_id {
            if &old_device_id != new_id {
                let _ = self.event_tx.send(TrackEvent::DeviceChanged(
                    track_id,
                    old_device_id,
                    new_id.clone(),
                ));
            }
        }
        
        let _ = self.event_tx.send(TrackEvent::ConfigUpdated(track_id));
        
        Ok(())
    }
    
    /// Set track mute state
    pub fn set_muted(&self, track_id: u8, muted: bool) -> Result<(), TrackError> {
        let track = self.tracks
            .get(&track_id)
            .ok_or(TrackError::NotFound(track_id))?;
        
        track.set_muted(muted);
        Ok(())
    }
    
    /// Set track solo state
    pub fn set_solo(&self, track_id: u8, solo: bool) -> Result<(), TrackError> {
        let track = self.tracks
            .get(&track_id)
            .ok_or(TrackError::NotFound(track_id))?;
        
        track.set_solo(solo);
        self.update_solo_state();
        
        Ok(())
    }
    
    /// Update global solo state
    fn update_solo_state(&self) {
        let any_solo = self.tracks
            .iter()
            .any(|entry| entry.is_solo());
        
        self.solo_active.store(any_solo, Ordering::Relaxed);
    }
    
    /// Check if track should output audio (considering solo/mute)
    pub fn should_output(&self, track_id: u8) -> bool {
        if let Some(track) = self.tracks.get(&track_id) {
            if track.is_muted() {
                return false;
            }
            
            if self.solo_active.load(Ordering::Relaxed) {
                return track.is_solo();
            }
            
            true
        } else {
            false
        }
    }
    
    /// Get all track statuses
    pub fn get_all_statuses(&self) -> Vec<TrackStatus> {
        self.tracks
            .iter()
            .map(|entry| entry.status())
            .collect()
    }
    
    /// Get track count
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }
    
    /// Get all track IDs
    pub fn track_ids(&self) -> Vec<u8> {
        self.tracks.iter().map(|e| *e.key()).collect()
    }
    
    /// Iterate over all tracks
    pub fn for_each<F>(&self, f: F)
    where
        F: Fn(&Track),
    {
        for entry in self.tracks.iter() {
            f(entry.value());
        }
    }
    
    /// Iterate over all tracks mutably
    pub fn for_each_mut<F>(&self, f: F)
    where
        F: Fn(&mut Track),
    {
        for mut entry in self.tracks.iter_mut() {
            f(entry.value_mut());
        }
    }
}

impl Default for TrackManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TrackType;
    
    #[test]
    fn test_create_track() {
        let manager = TrackManager::new();
        
        let config = TrackConfig {
            track_id: None,
            name: "Test Track".to_string(),
            device_id: "test".to_string(),
            bitrate: 128000,
            frame_size_ms: 10.0,
            channels: 2,
            track_type: TrackType::Music,
            fec_enabled: false,
        };
        
        let id = manager.create_track(config).unwrap();
        assert_eq!(id, 0);
        assert_eq!(manager.track_count(), 1);
    }
    
    #[test]
    fn test_remove_track() {
        let manager = TrackManager::new();
        
        let config = TrackConfig::default();
        let id = manager.create_track(config).unwrap();
        
        assert!(manager.remove_track(id).is_ok());
        assert_eq!(manager.track_count(), 0);
    }
    
    #[test]
    fn test_mute_solo() {
        let manager = TrackManager::new();
        
        let config1 = TrackConfig::default();
        let config2 = TrackConfig::default();
        
        let id1 = manager.create_track(config1).unwrap();
        let id2 = manager.create_track(config2).unwrap();
        
        // Test mute
        manager.set_muted(id1, true).unwrap();
        assert!(!manager.should_output(id1));
        assert!(manager.should_output(id2));
        
        // Test solo
        manager.set_muted(id1, false).unwrap();
        manager.set_solo(id1, true).unwrap();
        assert!(manager.should_output(id1));
        assert!(!manager.should_output(id2));
    }
}
