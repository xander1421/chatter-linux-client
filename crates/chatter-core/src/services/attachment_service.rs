//! Attachment service.
//!
//! This module re-exports from the modular attachment service.
//! See `attachment/` for the implementation.

pub use crate::services::attachment::{
    AttachmentError, AttachmentService, CdnUploadResult,
};
