//! Domain to Slint type conversions.
//!
//! Pure functions for converting domain types to Slint UI types.
//!
//! Note: Some Slint types contain `slint::Image` which is not `Send`,
//! so we provide Send-safe intermediate types for use across thread boundaries.

mod utils;
mod channel;
mod attachment;
mod message;
pub(crate) mod markdown;

pub use utils::*;
pub use channel::*;
pub use attachment::*;
pub use message::*;
