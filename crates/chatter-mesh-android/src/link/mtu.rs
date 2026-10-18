/// Bytes of ATT overhead on every write: one opcode, two for the handle.
pub const ATT_WRITE_OVERHEAD: i32 = 3;

/// The ATT MTU every BLE connection starts on, per the Bluetooth spec.
pub const MIN_ATT_MTU: i32 = 23;

/// The largest ATT MTU Android will negotiate, and what we ask for on connect.
pub const MAX_ATT_MTU: i32 = 517;

/// Payload bytes available on a link whose ATT MTU is `mtu`.
///
/// The one number in this file that must never be a constant or a guess:
/// fragment sizing depends on it, and an oversized write is dropped by the
/// controller with no error reported anywhere. Clamped at both ends because a
/// value outside the spec's range says the reporter is confused, not that the
/// radio can do something impossible.
pub const fn max_write_for_mtu(mtu: i32) -> usize {
    // Saturating, because this is fed straight from a number Kotlin reported:
    // `i32::MIN - 3` is a debug-build panic, and a panic here would be reached
    // from a binder thread.
    let usable = mtu.saturating_sub(ATT_WRITE_OVERHEAD);
    if usable < MIN_ATT_MTU - ATT_WRITE_OVERHEAD {
        (MIN_ATT_MTU - ATT_WRITE_OVERHEAD) as usize
    } else if usable > MAX_ATT_MTU - ATT_WRITE_OVERHEAD {
        (MAX_ATT_MTU - ATT_WRITE_OVERHEAD) as usize
    } else {
        usable as usize
    }
}

/// What a link can carry before `onMtuChanged` has been heard from: 23 − 3.
///
/// Assumed for every new link. Starting optimistic and correcting downward
/// would mean silently truncating every packet sent in the meantime.
pub const DEFAULT_MAX_WRITE: usize = max_write_for_mtu(MIN_ATT_MTU);

/// The most any link can carry: 517 − 3.
pub const LARGEST_MAX_WRITE: usize = max_write_for_mtu(MAX_ATT_MTU);

/// Constrain a size reported by Kotlin to something a BLE link can actually be.
///
/// Clamping *up* to [`DEFAULT_MAX_WRITE`] is safe rather than optimistic: the
/// Bluetooth spec guarantees an ATT MTU of at least 23, so 20 payload bytes
/// always fit. Clamping *down* to [`LARGEST_MAX_WRITE`] is the important half —
/// an inflated number would be believed by the fragmenter and every packet
/// built from it would vanish inside the controller.
pub fn sanitise_max_write(reported: i32) -> usize {
    reported.clamp(DEFAULT_MAX_WRITE as i32, LARGEST_MAX_WRITE as i32) as usize
}
