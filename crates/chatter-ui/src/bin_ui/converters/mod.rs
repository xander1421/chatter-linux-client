//! Domain to Slint type conversions.
//!
//! Pure functions for converting domain types to Slint UI types.
//!
//! Note: Some Slint types contain `slint::Image` which is not `Send`,
//! so we provide Send-safe intermediate types for use across thread boundaries.

mod attachment;
pub(crate) mod channel;
mod kind;
mod message;
#[cfg(test)]
mod open_bench;
mod registration_avatar;
mod utils;

pub use attachment::*;
pub use channel::*;
pub use chatter_core::ui::sidebar_data::{ChatRowSendData, FriendDataSendData};
pub use kind::*;
pub use message::*;
pub use registration_avatar::*;
pub use utils::*;
