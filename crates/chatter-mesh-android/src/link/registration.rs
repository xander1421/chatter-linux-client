use super::*;

/// The link the JNI callbacks currently report into.
///
/// A `Weak`, not an `Arc`: the native method table is registered once for the
/// whole process, so callbacks have to find their way to a link through a
/// global — but holding a strong reference here would keep a dropped
/// `AndroidLink` alive forever, still advertising, with nobody draining it.
static ACTIVE: Mutex<Option<Weak<Shared>>> = Mutex::new(None);

pub(super) fn set_active(shared: &Arc<Shared>) {
    *ACTIVE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(shared));
}

pub(super) fn clear_active(shared: &Arc<Shared>) {
    let mut slot = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    let ours = slot
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|current| Arc::ptr_eq(&current, shared));
    if ours {
        *slot = None;
    }
}

/// The link a callback should report into, if there is one.
fn active(during: &str) -> Option<Arc<Shared>> {
    let shared = ACTIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    if shared.is_none() {
        error::log_warn(&format!(
            "mesh: {during} arrived with no AndroidLink attached, dropped"
        ));
    }
    shared
}

/// Push one fully-formed event and apply any state change it implies.
///
/// Used for the two events that need nothing from the connection table.
pub(crate) fn dispatch(event: LinkEvent) {
    let Some(shared) = active(&describe(&event)) else {
        return;
    };
    let powered_off = matches!(event, LinkEvent::AdapterPowerChanged { powered: false });
    shared.emit(event);
    if powered_off {
        // Every connection died with the controller. Say so once per link
        // rather than leaving the layer above to discover it one failed write
        // at a time.
        shared.drop_all();
    }
}

/// `nativeOnConnected`: this connection is writable now.
pub(crate) fn on_connected(handle: &str, role: LinkRole, reported_max_write: i32) {
    let Some(shared) = active("nativeOnConnected") else {
        return;
    };
    shared.establish(role, handle, sanitise_max_write(reported_max_write));
}

/// `nativeOnDisconnected`: this connection is gone.
pub(crate) fn on_disconnected(handle: &str) {
    let Some(shared) = active("nativeOnDisconnected") else {
        return;
    };
    shared.close_link(handle);
}

/// `nativeOnData`: bytes arrived on this connection.
pub(crate) fn on_data(handle: &str, bytes: Vec<u8>) {
    let Some(shared) = active("nativeOnData") else {
        return;
    };
    shared.inbound(handle, bytes);
}
