// Copyright © Chatter contributors
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

import java.util.IdentityHashMap;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;

/** UI-thread lifecycle policy; the host reads current root insets, never a cached frame. */
final class InsetsRecovery {
    interface Host {
        boolean ready();
        void requestInsets();
        void postAfterLayout(Runnable action);
        void publishCurrent();
        void invalidateQueuedInsets(long sequence);
    }

    // Shared across helper recreation: callbacks from a retired Activity cannot reuse a sequence.
    private static final AtomicLong sequence = new AtomicLong();
    static long nextSequence() { return sequence.incrementAndGet(); }
    private static InsetsRecovery currentOwner;
    private final Host host;
    private long lifecycle;
    private boolean focused;
    private final IdentityHashMap<Object, Long> animations = new IdentityHashMap<>();

    InsetsRecovery(Host host) { this.host = host; }

    void focusChanged(boolean focused) {
        this.focused = focused;
        ++lifecycle;
        animations.clear();
        if (focused) {
            currentOwner = this;
            host.invalidateQueuedInsets(nextSequence());
            reconcile();
        } else if (currentOwner == this) {
            currentOwner = null;
            host.invalidateQueuedInsets(nextSequence());
        }
    }

    void detached() { focusChanged(false); }

    boolean canPublish() { return focused && currentOwner == this && host.ready(); }

    void prepare(Object animation) { animations.put(animation, lifecycle); }

    boolean progress(List<?> running) {
        if (!canPublish() || running.isEmpty()) return false;
        for (Object animation : running) {
            Long owner = animations.get(animation);
            if (owner == null || owner.longValue() != lifecycle) return false;
        }
        return true;
    }

    void end(Object animation) {
        Long owner = animations.remove(animation);
        if (owner != null && owner.longValue() == lifecycle && animations.isEmpty()) reconcile();
    }

    void regainedFocus() { focusChanged(host.ready()); }

    void returnToWindow() {
        if (!focused && host.ready()) focusChanged(true);
        else reconcile();
    }

    void reconcile() {
        if (!canPublish() || !animations.isEmpty()) return;
        final long owner = lifecycle;
        host.requestInsets();
        // Immediate recovery repairs an already laid-out return; the posted pull
        // catches the subsequent layout/resize without retaining its old snapshot.
        host.publishCurrent();
        host.postAfterLayout(() -> {
            if (owner == lifecycle && canPublish() && animations.isEmpty()) host.publishCurrent();
        });
    }
}
