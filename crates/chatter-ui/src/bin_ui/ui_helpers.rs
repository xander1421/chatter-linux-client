//! Shared UI helper macros for async modal callbacks.
//!
//! Eliminates the repeated `tokio::spawn` + `invoke_from_event_loop` +
//! loading state boilerplate found in every modal callback.

/// Default timeout for UI-spawned service calls (10 seconds).
///
/// Long enough for slow networks, short enough that users won't force-quit.
pub const UI_SERVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run an async action with loading state management.
///
/// Handles the common modal callback pattern:
/// 1. Set loading = true
/// 2. Run async work
/// 3. On success: set loading = false, run success closure
/// 4. On error: set loading = false, log error
///
/// # Parameters
/// - `$weak`: `slint::Weak<MainWindow>` — weak reference to the window
/// - `$set_loading`: method name on MainWindow to set loading state (e.g. `set_profile_loading`)
/// - `$action`: async expression that returns `Result<T, E>`
/// - `$on_success`: closure `|w: &MainWindow, result: T|` called on UI thread on success
///
/// # Example
/// ```ignore
/// spawn_async_action!(weak, set_profile_loading,
///     async { user_service.remove_friend(&friend_id).await },
///     |w, _| { w.set_profile_is_friend(false); }
/// );
/// ```
#[macro_export]
macro_rules! spawn_async_action {
    ($weak:expr, $set_loading:ident, $action:expr, $on_success:expr) => {{
        let weak = $weak.clone();

        // Set loading = true
        let weak_loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_loading.upgrade() {
                w.$set_loading(true);
            }
        });

        tokio::spawn(async move {
            let result = tokio::time::timeout(
                $crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                async { $action },
            ).await;

            match result {
                Ok(Ok(val)) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.$set_loading(false);
                            #[allow(clippy::redundant_closure_call)]
                            ($on_success)(&w, val);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Async action failed");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.$set_loading(false);
                        }
                    });
                }
                Err(_elapsed) => {
                    tracing::error!("Async action timed out after {:?}", $crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.$set_loading(false);
                        }
                    });
                }
            }
        });
    }};
}
