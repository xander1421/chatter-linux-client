//! Store permanent send refusal separately from delivery state.
use super::Migration;
pub struct V075MessageFailureReason;
impl Migration for V075MessageFailureReason {
    fn version(&self) -> u32 { 75 }
    fn description(&self) -> &'static str { "Store permanent send failure reason" }
    fn up(&self) -> &'static str { "ALTER TABLE messages ADD COLUMN failure_reason TEXT;" }
    fn down(&self) -> Option<&'static str> { None }
}
