//! Migration: Create unified reactions table.
//!
//! NOTE: This migration is now a no-op. The unified_reactions table is created
//! by v045 which does a full DROP+CREATE of all message and reaction tables.
//! Kept as a registered migration so the version number is tracked.

use super::Migration;

pub struct V044UnifiedReactions;

impl Migration for V044UnifiedReactions {
    fn version(&self) -> u32 {
        44
    }

    fn description(&self) -> &'static str {
        "No-op: unified_reactions created by v045"
    }

    fn up(&self) -> &'static str {
        "-- No-op: unified_reactions table is created by v045"
    }
}
