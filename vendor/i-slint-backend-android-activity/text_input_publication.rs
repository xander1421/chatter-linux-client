// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Publication bookkeeping independent of JNI. A failed delivery leaves the
//! current generation owed; an eventual push replaces a reset that never landed.
use std::cell::Cell;

#[derive(Default)]
pub(crate) struct Publication {
    reset_pending: Cell<bool>,
    publish_pending: Cell<bool>,
}

impl Publication {
    pub(crate) fn reset_pending(&self) -> bool {
        self.reset_pending.get()
    }
    pub(crate) fn publish_pending(&self) -> bool {
        self.publish_pending.get()
    }
    pub(crate) fn begin_reset(&self) {
        self.reset_pending.set(true);
    }
    pub(crate) fn clear_reset(&self) {
        self.reset_pending.set(false);
    }
    pub(crate) fn forget(&self) {
        self.clear_reset();
        self.publish_pending.set(false);
    }

    pub(crate) fn pushed(&self, reached: bool) {
        if reached {
            if self.publish_pending.replace(false) {
                self.reset_pending.set(false);
            }
        } else {
            self.publish_pending.set(true);
        }
    }

    pub(crate) fn failed_consume(&self) {
        self.publish_pending.set(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_consume_retries_until_push_lands() {
        let state = Publication::default();
        state.begin_reset();
        state.failed_consume();
        // Inject the JNI outcome, including a second failure on the retry.
        let mut outcomes = [false, true].into_iter();
        state.pushed(outcomes.next().unwrap());
        assert!(state.publish_pending());
        assert!(state.reset_pending());
        state.pushed(outcomes.next().unwrap());
        assert!(!state.publish_pending());
        assert!(!state.reset_pending());
    }

    #[test]
    fn successful_ordinary_push_does_not_acknowledge_a_reset() {
        let state = Publication::default();
        state.begin_reset();
        state.pushed(true);
        assert!(state.reset_pending());
        assert!(!state.publish_pending());
    }

    #[test]
    fn failed_restore_is_owed_without_introducing_a_reset() {
        let state = Publication::default();
        state.pushed(false);
        assert!(state.publish_pending());
        assert!(!state.reset_pending());
        state.pushed(true);
        assert!(!state.publish_pending());
        assert!(!state.reset_pending());
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    #[test]
    fn switching_fields_discards_old_publication_and_reset() {
        let state = Publication::default();
        state.begin_reset();
        state.failed_consume();
        state.forget();
        assert!(!state.reset_pending());
        assert!(!state.publish_pending());
    }
    #[test]
    fn reset_ack_does_not_cancel_a_failed_newer_push() {
        let state = Publication::default();
        state.begin_reset();
        state.pushed(false);
        state.clear_reset();
        assert!(!state.reset_pending());
        assert!(state.publish_pending());
        state.pushed(true);
        assert!(!state.publish_pending());
    }
}
