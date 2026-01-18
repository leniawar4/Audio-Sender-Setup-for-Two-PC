//! Track management module

pub mod manager;
pub mod track;

pub use manager::{TrackManager, TrackEvent};
pub use track::{Track, TrackState};
