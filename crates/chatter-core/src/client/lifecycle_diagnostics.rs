//! Constant metadata only; identifies remaining cold-start lifecycle holders.
macro_rules! phases {
    ($($phase:ident => ($requested:literal, $acquired:literal, $ending:literal)),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub(super) enum Phase { $($phase),+ }
        impl Phase {
            pub(super) fn requested(self) {
                match self { $(Self::$phase => tracing::info!(target: $requested, "Lifecycle diagnostic")),+ }
            }
            pub(super) fn acquired(self) -> Scope {
                match self { $(Self::$phase => tracing::info!(target: $acquired, "Lifecycle diagnostic")),+ }
                Scope(self)
            }
        }
        impl Drop for Scope {
            fn drop(&mut self) {
                match self.0 { $(Phase::$phase => tracing::info!(target: $ending, "Lifecycle diagnostic")),+ }
            }
        }
    };
}
pub(super) struct Scope(Phase);
phases! {
    PushWakeup => ("call_setup::push_lifecycle_requested", "call_setup::push_lifecycle_acquired", "call_setup::push_lifecycle_scope_ending"),
    PushIdentityRefresh => ("call_setup::push_identity_requested", "call_setup::push_identity_acquired", "call_setup::push_identity_scope_ending"),
    NotificationStart => ("call_setup::notification_start_requested", "call_setup::notification_start_acquired", "call_setup::notification_start_scope_ending"),
    NotificationRetire => ("call_setup::notification_retire_requested", "call_setup::notification_retire_acquired", "call_setup::notification_retire_scope_ending"),
}
