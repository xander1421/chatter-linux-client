//! Translating BlueZ and socket failures into the link layer's error vocabulary.
//!
//! The classification here is load bearing rather than cosmetic. [`LinkError`]
//! draws one distinction the layer above acts on directly:
//!
//! - [`LinkError::WriteRejected`] means *try again* — the controller queue is
//!   full, which happens constantly under load. There is no retransmission
//!   below us, so a caller that discards on rejection loses the packet outright.
//! - [`LinkError::LinkGone`] means *stop* — drop every piece of state keyed on
//!   this link and do not retry.
//!
//! Getting these two backwards is invisible in a code review and produces either
//! a mesh that silently drops traffic under load, or one that spins forever
//! writing into a socket whose peer walked away.

use std::io;

use bluer::ErrorKind;
use chatter_mesh_link::{LinkError, LinkId};

/// `ENOBUFS`. The kernel ran out of socket buffer space for this send.
///
/// Rust maps this to [`io::ErrorKind::Uncategorized`], which is unstable and
/// cannot be matched on, so it has to be recognised by its raw errno. It is a
/// backpressure signal, not a dead link, and Linux is the only platform this
/// crate compiles for so the constant is unambiguous.
const ENOBUFS: i32 = 105;

/// Does this BlueZ failure mean the adapter itself is not usable?
fn is_adapter_fault(kind: &ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::NotFound
            | ErrorKind::NotReady
            | ErrorKind::NotAvailable
            | ErrorKind::DoesNotExist
            | ErrorKind::NotSupported
    )
}

/// Classify a failure from an adapter-level operation — session setup, power,
/// discovery.
pub fn classify_adapter(err: &bluer::Error) -> LinkError {
    if is_adapter_fault(&err.kind) {
        LinkError::AdapterUnavailable(err.to_string())
    } else {
        LinkError::Backend(err.to_string())
    }
}

/// Classify a failure from publishing the peripheral role: the advertisement or
/// the GATT application.
pub fn classify_peripheral(err: &bluer::Error) -> LinkError {
    LinkError::PeripheralSetup(err.to_string())
}

/// Classify a failure from an outbound connect attempt.
///
/// Everything on this path collapses to [`LinkError::ConnectFailed`]: from the
/// caller's point of view "the adapter is gone" and "the peer refused" are the
/// same event — no link came up, and the scheduler picks a different candidate.
pub fn classify_connect(address: &str, err: &bluer::Error) -> LinkError {
    LinkError::ConnectFailed {
        address: address.to_string(),
        reason: err.to_string(),
    }
}

/// A connect attempt that failed for a reason BlueZ did not report, e.g. a
/// malformed address or a service that the peer does not actually expose.
pub fn connect_failed(address: &str, reason: impl Into<String>) -> LinkError {
    LinkError::ConnectFailed {
        address: address.to_string(),
        reason: reason.into(),
    }
}

/// Classify a failure writing to a characteristic socket.
///
/// See the module docs: the split between retryable and terminal is the whole
/// point of this function.
pub fn classify_write(link: &LinkId, err: &io::Error) -> LinkError {
    match err.kind() {
        // The socket to bluetoothd is full, or the write was interrupted before
        // anything went out. Both are ordinary congestion.
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::OutOfMemory => {
            LinkError::WriteRejected(link.to_string())
        }
        // bluetoothd closed its end: the ACL dropped, or the peer tore down the
        // notification session. The link is not coming back under this id.
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::UnexpectedEof => LinkError::LinkGone(link.to_string()),
        _ if err.raw_os_error() == Some(ENOBUFS) => LinkError::WriteRejected(link.to_string()),
        _ => LinkError::Backend(format!("write on link {link} failed: {err}")),
    }
}

/// Does this error mean the caller should tear the link down?
pub fn is_terminal(err: &LinkError) -> bool {
    matches!(err, LinkError::LinkGone(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> LinkId {
        LinkId::new("central:/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF#1")
    }

    #[test]
    fn a_full_controller_queue_is_retryable_not_fatal() {
        // This is the single most important mapping in the file. BLE writes are
        // refused routinely; treating that as a dead link would tear down every
        // busy peer in the room.
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Interrupted,
            io::ErrorKind::OutOfMemory,
        ] {
            let err = classify_write(&link(), &io::Error::new(kind, "queue full"));
            assert_eq!(
                err,
                LinkError::WriteRejected(link().to_string()),
                "{kind:?}"
            );
            assert!(!is_terminal(&err));
        }
    }

    #[test]
    fn enobufs_is_retryable_even_though_rust_cannot_name_it() {
        let err = classify_write(&link(), &io::Error::from_raw_os_error(ENOBUFS));
        assert_eq!(err, LinkError::WriteRejected(link().to_string()));
    }

    #[test]
    fn a_closed_socket_is_terminal() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
            io::ErrorKind::UnexpectedEof,
        ] {
            let err = classify_write(&link(), &io::Error::new(kind, "gone"));
            assert_eq!(err, LinkError::LinkGone(link().to_string()), "{kind:?}");
            assert!(is_terminal(&err));
        }
    }

    #[test]
    fn an_unrecognised_write_failure_is_neither_retried_nor_treated_as_a_dead_link() {
        let err = classify_write(
            &link(),
            &io::Error::new(io::ErrorKind::InvalidInput, "nonsense"),
        );
        assert!(matches!(err, LinkError::Backend(_)));
        assert!(!is_terminal(&err));
    }

    #[test]
    fn missing_hardware_is_reported_as_an_adapter_fault() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::NotReady,
            ErrorKind::NotAvailable,
            ErrorKind::DoesNotExist,
            ErrorKind::NotSupported,
        ] {
            let err = classify_adapter(&bluer::Error {
                kind: kind.clone(),
                message: "no adapter".into(),
            });
            assert!(matches!(err, LinkError::AdapterUnavailable(_)), "{kind:?}");
        }
    }

    #[test]
    fn other_bluez_faults_stay_unclassified_rather_than_blaming_the_adapter() {
        let err = classify_adapter(&bluer::Error {
            kind: ErrorKind::InProgress,
            message: "busy".into(),
        });
        assert!(matches!(err, LinkError::Backend(_)));
    }

    #[test]
    fn connect_failures_carry_the_address_they_were_for() {
        let err = classify_connect(
            "AA:BB:CC:DD:EE:FF",
            &bluer::Error {
                kind: ErrorKind::ConnectionAttemptFailed,
                message: "le-connection-abort".into(),
            },
        );
        match err {
            LinkError::ConnectFailed { address, reason } => {
                assert_eq!(address, "AA:BB:CC:DD:EE:FF");
                assert!(reason.contains("le-connection-abort"));
            }
            other => panic!("expected ConnectFailed, got {other:?}"),
        }
    }

    #[test]
    fn peripheral_setup_failures_are_distinguishable_from_link_failures() {
        let err = classify_peripheral(&bluer::Error {
            kind: ErrorKind::AlreadyExists,
            message: "adv in use".into(),
        });
        assert!(matches!(err, LinkError::PeripheralSetup(_)));
    }
}
