package com.google.androidgamesdk;

import android.os.Handler;
import android.os.Looper;
import android.view.KeyEvent;
import android.view.View;
import android.view.inputmethod.CompletionInfo;
import android.view.inputmethod.CorrectionInfo;
import android.view.inputmethod.EditorInfo;
import androidx.core.graphics.Insets;
import com.google.androidgamesdk.gametextinput.InputConnection;
import com.google.androidgamesdk.gametextinput.Listener;
import com.google.androidgamesdk.gametextinput.Settings;
import com.google.androidgamesdk.gametextinput.State;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The producer gate for the Slint text field's native copy.
 *
 * GameTextInput keeps the IME's copy of the focused field in an {@link
 * InputConnection}'s Editable on the UI thread. Slint keeps its own on the
 * native thread. An accepted send consumes the Slint copy at once, but the
 * Editable could only be reset by a state posted to the UI thread, and in the
 * meantime the IME kept producing snapshots of the consumed text (Gboard
 * re-marks the last word as composing after every restart) which the native
 * side could not tell from new typing: it took them as full replacements and
 * the sent text came back into the composer (RC88, Galaxy S9).
 *
 * This subclass is the one InputConnection the IME talks to and the one
 * GameActivity registers with its native side: {@link GatedSurfaceView}
 * installs it while the surface view is constructed, before
 * {@code GameActivity.onCreate} calls {@code setInputConnectionNative}, so the
 * native show/hide/restart calls land on the gate and its
 * {@code softKeyboardActive} flag is the one {@code onCreateInputConnection}
 * consults. It is the point where the two copies are ordered:
 *
 * <ul>
 * <li>Native accepts a send: {@link #publishReset} stores an immutable pending
 *     reset {@code (owner, generation)} at once, from any thread, with no lock
 *     and no wait on the UI thread, and bumps a publication counter.</li>
 * <li>Every mutating IME operation runs through {@link #gated}: on the UI
 *     thread it claims a pending reset first (the Editable is emptied and
 *     input restarted), then runs; if a reset was published while it ran,
 *     the operation had landed on the consumed text, so the reset is claimed
 *     and the operation runs again on the new generation. A posted fallback
 *     claims a reset no operation came for. Claims are once-only
 *     (compare-and-set) and nested calls (setState's own setSelection,
 *     processKeyEvent's parts) are not gated again.</li>
 * <li>A claim applies the empty state, tells GameActivity's native copy
 *     (setState itself notifies nobody) and acknowledges the generation to
 *     Slint; the acknowledgement carries no text and never clears anything.</li>
 * <li>Every snapshot the IME produces is tagged with the owner and the
 *     generation applied when it was produced ({@link #stateChanged}), so the
 *     native side drops snapshots from before the reset by provenance, never
 *     by content.</li>
 * <li>Every outbound Slint state ({@link #push}) is versioned by owner and
 *     generation, both monotonic: a push for an older field is ignored, a
 *     push issued before the accepted send of the same field claims the reset
 *     and is dropped, and a push establishes its field's owner before any
 *     claim for that field can happen.</li>
 * </ul>
 *
 * In this package so it can replace {@code InputEnabledSurfaceView.mInputConnection}
 * (package-private) the way the view's own constructor filled it.
 */
public final class ChatterTextInputGate extends InputConnection implements Listener {
    /** An accepted send's consumption of the field {@code owner}, immutable. */
    private static final class PendingReset {
        final long owner;
        final long generation;

        PendingReset(long owner, long generation) {
            this.owner = owner;
            this.generation = generation;
        }
    }

    private interface Operation {
        boolean run();
    }

    private static final State EMPTY = new State("", 0, 0, -1, -1);
    private static final AtomicReference<PendingReset> PENDING = new AtomicReference<>();
    /** Bumped on every publication, so an operation can see one that landed while it ran. */
    private static final AtomicLong PUBLISHED = new AtomicLong();
    private static final Handler MAIN = new Handler(Looper.getMainLooper());
    private static volatile ChatterTextInputGate installed;

    private final GameActivity activity;
    /** UI thread only: the field the IME serves, as native numbers it; monotonic. */
    private long owner;
    /** UI thread only: the generation the Editable reflects (last reset or push). */
    private long generation;
    /** UI thread only: > 0 while a gated operation, a reset or a push is being applied. */
    private int depth;

    private ChatterTextInputGate(GameActivity activity, View view, Settings settings) {
        super(activity, view, settings);
        this.activity = activity;
    }

    /**
     * GameActivity's surface view with the gate as its InputConnection from
     * construction on. GameActivity creates the view in {@code onCreateSurfaceView}
     * and only afterwards registers {@code mSurfaceView.mInputConnection} with
     * native code, so the gate is what native code holds.
     */
    public static class GatedSurfaceView extends GameActivity.InputEnabledSurfaceView {
        public GatedSurfaceView(GameActivity activity) {
            activity.super(activity);
            // The connection the superclass constructor built; same settings as
            // it derived (key events forwarded for an untyped field).
            InputConnection previous = mInputConnection;
            EditorInfo info = previous != null ? previous.getEditorInfo() : activity.getImeEditorInfo();
            ChatterTextInputGate gate =
                    new ChatterTextInputGate(activity, this, new Settings(info, info.inputType == 0));
            gate.setListener(gate);
            mInputConnection = gate;
            installed = gate;
        }
    }

    // ---- Native side (any thread, never waits) ------------------------------

    /** Native accepted a send of the field {@code owner}: its Editable is consumed as of now. */
    public static void publishReset(long owner, long generation) {
        PENDING.set(new PendingReset(owner, generation));
        PUBLISHED.incrementAndGet();
        MAIN.post(ChatterTextInputGate::claimFromHandler);
    }

    /** Slint's state for the field {@code owner}, computed at {@code generation}. */
    public static void push(
            String text, int selStart, int selEnd, int compStart, int compEnd, long owner, long generation) {
        MAIN.post(() -> {
            ChatterTextInputGate gate = installed;
            if (gate != null) {
                gate.applyPush(text, selStart, selEnd, compStart, compEnd, owner, generation);
            }
        });
    }

    private static void claimFromHandler() {
        ChatterTextInputGate gate = installed;
        if (gate != null) {
            gate.claim();
        }
    }

    // ---- UI thread ---------------------------------------------------------

    /**
     * Apply the pending reset if it is this field's; once. A reset for a field
     * older than the one the IME serves is moot and dropped; one for a newer
     * field waits for that field's first push, which establishes the owner.
     * Returns whether a reset was applied.
     */
    private boolean claim() {
        PendingReset reset = PENDING.get();
        if (reset == null) {
            return false;
        }
        if (reset.owner < owner) {
            PENDING.compareAndSet(reset, null);
            return false;
        }
        if (reset.owner > owner) {
            return false;
        }
        if (!PENDING.compareAndSet(reset, null)) {
            return false;
        }
        applyReset(reset.generation);
        return true;
    }

    private void applyReset(long resetGeneration) {
        generation = resetGeneration;
        depth++;
        try {
            setState(EMPTY);
        } finally {
            depth--;
        }
        // setState notifies nobody: refresh GameActivity's native copy of the
        // state, then acknowledge the generation to Slint. The acknowledgement
        // carries no text; Slint's own copy is left as it is.
        activity.stateChanged(EMPTY, false);
        nativeTextInputAck(owner, resetGeneration);
    }

    private void applyPush(
            String text, int selStart, int selEnd, int compStart, int compEnd, long pushOwner, long pushGeneration) {
        if (pushOwner < owner) {
            // A field the IME no longer serves: never resurrect it.
            return;
        }
        // Establish the owner first, so a reset published for this field
        // before its first push arrived can be claimed from here on.
        owner = pushOwner;
        PendingReset reset = PENDING.get();
        if (reset != null && reset.owner == owner) {
            if (pushGeneration < reset.generation) {
                // Issued before the send was accepted: the reset supersedes it.
                claim();
                return;
            }
            // Already reflects the consumption.
            PENDING.compareAndSet(reset, null);
        } else if (reset != null && reset.owner < owner) {
            // The IME moved to a newer field since that send was accepted.
            PENDING.compareAndSet(reset, null);
        }
        if (pushGeneration < generation) {
            return;
        }
        generation = pushGeneration;
        depth++;
        try {
            setState(new State(text, selStart, selEnd, compStart, compEnd));
        } finally {
            depth--;
        }
    }

    /**
     * Run one of the IME's mutating operations after any pending reset, and
     * on the new generation if a reset was published while it ran. Nested
     * calls (setState's setSelection/setComposingRegion, processKeyEvent's
     * finishComposingText/setComposingRegion/setSelection) belong to the
     * outer operation and are not gated again.
     */
    private boolean gated(Operation operation) {
        if (depth > 0) {
            return operation.run();
        }
        depth++;
        try {
            long seen = PUBLISHED.get();
            claim();
            boolean result = operation.run();
            if (PUBLISHED.get() != seen && claim()) {
                // The operation landed on the consumed text, which the reset
                // just emptied: it is input for the new generation, run it there.
                result = operation.run();
            }
            return result;
        } finally {
            depth--;
        }
    }

    // ---- The IME's operations ------------------------------------------------

    @Override
    public boolean setComposingText(CharSequence text, int newCursorPosition) {
        return gated(() -> super.setComposingText(text, newCursorPosition));
    }

    @Override
    public boolean setComposingRegion(int start, int end) {
        return gated(() -> super.setComposingRegion(start, end));
    }

    @Override
    public boolean finishComposingText() {
        return gated(super::finishComposingText);
    }

    @Override
    public boolean commitText(CharSequence text, int newCursorPosition) {
        return gated(() -> super.commitText(text, newCursorPosition));
    }

    @Override
    public boolean commitCompletion(CompletionInfo text) {
        return gated(() -> super.commitCompletion(text));
    }

    @Override
    public boolean commitCorrection(CorrectionInfo correctionInfo) {
        return gated(() -> super.commitCorrection(correctionInfo));
    }

    @Override
    public boolean deleteSurroundingText(int beforeLength, int afterLength) {
        return gated(() -> super.deleteSurroundingText(beforeLength, afterLength));
    }

    @Override
    public boolean deleteSurroundingTextInCodePoints(int beforeLength, int afterLength) {
        return gated(() -> super.deleteSurroundingTextInCodePoints(beforeLength, afterLength));
    }

    @Override
    public boolean setSelection(int start, int end) {
        return gated(() -> super.setSelection(start, end));
    }

    @Override
    public boolean sendKeyEvent(KeyEvent event) {
        return gated(() -> super.sendKeyEvent(event));
    }

    @Override
    public boolean endBatchEdit() {
        // Edits nothing; reports the snapshot. Gated so a batch with no edit
        // still reports the new generation's state after a claim.
        return gated(super::endBatchEdit);
    }

    @Override
    public boolean onKey(View view, int keyCode, KeyEvent event) {
        return gated(() -> super.onKey(view, keyCode, event));
    }

    @Override
    public boolean performEditorAction(int actionCode) {
        // An action, not an edit: never run twice.
        if (depth == 0) {
            claim();
        }
        return super.performEditorAction(actionCode);
    }

    // ---- Listener: tag every snapshot, then GameActivity's own path ----------

    @Override
    public void stateChanged(State state, boolean dismissed) {
        nativeTextInputState(
                state.text,
                state.selectionStart,
                state.selectionEnd,
                state.composingRegionStart,
                state.composingRegionEnd,
                owner,
                generation);
        activity.stateChanged(state, dismissed);
    }

    @Override
    public void onImeInsetsChanged(Insets insets) {
        activity.onImeInsetsChanged(insets);
    }

    @Override
    public void onSoftwareKeyboardVisibilityChanged(boolean visible) {
        activity.onSoftwareKeyboardVisibilityChanged(visible);
    }

    @Override
    public void onEditorAction(int action) {
        activity.onEditorAction(action);
    }

    /** Implemented in Rust (chatter-ui platform/android/text_input.rs). UTF-16 offsets. */
    private static native void nativeTextInputState(
            String text, int selStart, int selEnd, int compStart, int compEnd, long owner, long generation);

    /** Implemented in Rust: a reset was applied on the UI thread. Carries no text. */
    private static native void nativeTextInputAck(long owner, long generation);
}
