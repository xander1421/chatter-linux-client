//! Domain to Slint type conversions.
//!
//! Pure functions for converting domain types to Slint UI types.
//!
//! Note: Some Slint types contain `slint::Image` which is not `Send`,
//! so we provide Send-safe intermediate types for use across thread boundaries.

mod attachment;
mod channel;
mod kind;
pub(crate) mod markdown;
mod message;
#[cfg(test)]
mod open_bench;
mod utils;

pub use attachment::*;
pub use channel::*;
pub use chatter_core::ui::sidebar_data::{
    ChatRowSendData, FriendDataSendData, FriendRequestSendData,
};
pub use kind::*;
pub use message::*;
pub use utils::*;
