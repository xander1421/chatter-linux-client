//! Call management module.
//!
//! Provides video/audio calling functionality via LiveKit integration.

mod room_manager;

pub use room_manager::{RoomManager, RoomState, RoomEvent as LiveKitRoomEvent};
