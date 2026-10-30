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
 * consults. It is the point where the two copies are ordered, by one monitor
 * ({@link #LOCK}):
 *
 * <ul>
 * <li>The native thread publishes two kinds of immutable request into slots
 *     under the monitor and returns: Slint's state for the field ({@link
 *     #push}; the latest replaces any earlier one still waiting) and an
 *     accepted send's consumption of it ({@link #publishReset}). It never
 *     waits on the UI thread's event loop; it waits at most for one IME
 *     operation to finish (a bounded monitor wait, see below).</li>
 * <li>On the UI thread, every mutating IME operation ({@link #gated}), the
 *     creation of the connection ({@link GatedSurfaceView#onCreateInputConnection})
 *     and a posted fallback each take the monitor, claim whatever is pending
 *     ({@link #claimAll}: the push first, which establishes the field's owner,
 *     then the reset) and only then run the operation. The operation's own
 *     snapshot publication (GameTextInput 4.4.0 publishes from
 *     {@code endBatchEdit} and {@code onKey} only, both gated) happens inside
 *     the same hold, so a request is either claimed before the operation or
 *     published after it completed: nothing is run twice and no operation
 *     lands between a claim and its application. Nested calls (setState's
 *     own setSelection/setComposingRegion/restartInput, processKeyEvent's
 *     parts) belong to the outer operation (depth guard).</li>
 * <li>A claim of a reset applies the empty state, tells GameActivity's native
 *     copy (setState itself notifies nobody) and acknowledges the generation
 *     to Slint; a push computed after the consumption applies instead of the
 *     reset and acknowledges it the same way with the state it applied. The
 *     acknowledgement carries no text and never clears anything.</li>
 * <li>Every snapshot the IME produces is tagged with the owner and the
 *     generation applied when it was produced ({@link #stateChanged}), so the
 *     native side drops snapshots from before the reset by provenance, never
 *     by content.</li>
 * <li>Owners and generations are monotonic: a push for an older field is
 *     ignored, a push issued before the accepted send of the same field claims
 *     the reset and is dropped, a push issued after it supersedes the reset
 *     (and acknowledges it), and a reset for a field the IME has not served
 *     yet waits for that field's first push, which the backend publishes
 *     before it asks native code to show the keyboard.</li>
 * </ul>
 *
 * Why a monitor is safe here (android-activity 0.6.1, games-activity 4.4.0):
 * the native thread reaches {@link #push}/{@link #publishReset} only from
 * Slint callbacks, and by then holds no lock the UI thread needs — the input
 * iterator's app lock ends before any callback ({@code game_activity/mod.rs}
 * {@code InputIteratorInner::from}, {@code lib.rs input_events_iter}; the C
 * buffer swap unlocks before returning) and the text-state copy under
 * {@code currentStateMutex_} completes before the event is dispatched. The UI
 * thread, holding the monitor, only enqueues to the Slint event loop (JNI
 * {@link #nativeTextInputState}/{@link #nativeTextInputAck} queue and wake),
 * briefly takes {@code currentStateMutex_} and the {@code android_app} mutex
 * inside {@code onTextInputEventNative} (no callbacks under them), and makes
 * the IMM {@code restartInput} binder call. Neither side waits for the other's
 * event loop.
 *
 * Activity replacement never happens inside one process: {@code android_main}
 * ends with {@code _exit(0)} (chatter-ui {@code lib.rs}), so a new Activity is
 * a new process with fresh statics and a fresh backend. Should a second gate
 * ever be constructed anyway, the earlier one is retired: it claims nothing
 * and publishes nothing.
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

    /** Slint's state for the field {@code owner}, computed at {@code generation}; immutable. */
    private static final class PendingPush {
        final State state;
        final long owner;
        final long generation;

        PendingPush(State state, long owner, long generation) {
            this.state = state;
            this.owner = owner;
            this.generation = generation;
        }
    }

    private interface Operation {
        boolean run();
    }

    private static final State EMPTY = new State("", 0, 0, -1, -1);
    /** Orders the native thread's publications with the UI thread's operations. */
    private static final Object LOCK = new Object();
    /** Guarded by {@link #LOCK}. */
    private static PendingReset pendingReset;
    /** Guarded by {@link #LOCK}. The latest push replaces an unclaimed earlier one. */
    private static PendingPush pendingPush;
    /** Guarded by {@link #LOCK}. */
    private static ChatterTextInputGate installed;
    private static final Handler MAIN = new Handler(Looper.getMainLooper());

    private final GameActivity activity;
    /** UI thread only: the field the IME serves, as native numbers it; monotonic. */
    private long owner;
    /** UI thread only: the generation the Editable reflects (last reset or push). */
    private long generation;
    /** UI thread only: > 0 while a gated operation or a claim is running. */
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
        private final ChatterTextInputGate gate;

        public GatedSurfaceView(GameActivity activity) {
            activity.super(activity);
            // The connection the superclass constructor built; same settings as
            // it derived (key events forwarded for an untyped field).
            InputConnection previous = mInputConnection;
            EditorInfo info = previous != null ? previous.getEditorInfo() : activity.getImeEditorInfo();
            gate = new ChatterTextInputGate(activity, this, new Settings(info, info.inputType == 0));
            gate.setListener(gate);
            mInputConnection = gate;
            synchronized (LOCK) {
                installed = gate;
            }
        }

        /**
         * The IME attaches (native show's restartInput, a window focus
         * change): the field's pending state and reset are applied first, so
         * the connection the IME gets already reflects them. A nested call
         * from that application's own restartInput is not a claim (depth).
         */
        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            gate.claimAllOutside();
            return super.onCreateInputConnection(outAttrs);
        }
    }

    // ---- Native side (any thread; bounded monitor wait, no event-loop wait) ----

    /** Native accepted a send of the field {@code owner}: its Editable is consumed as of now. */
    public static void publishReset(long owner, long generation) {
        synchronized (LOCK) {
            pendingReset = new PendingReset(owner, generation);
        }
        MAIN.post(ChatterTextInputGate::claimFromHandler);
    }

    /** Slint's state for the field {@code owner}, computed at {@code generation}. */
    public static void push(
            String text, int selStart, int selEnd, int compStart, int compEnd, long owner, long generation) {
        PendingPush push =
                new PendingPush(new State(text, selStart, selEnd, compStart, compEnd), owner, generation);
        synchronized (LOCK) {
            pendingPush = push;
        }
        MAIN.post(ChatterTextInputGate::claimFromHandler);
    }

    /** The fallback: whatever no IME operation came for is applied from here. */
    private static void claimFromHandler() {
        ChatterTextInputGate gate;
        synchronized (LOCK) {
            gate = installed;
        }
        if (gate != null) {
            gate.claimAllOutside();
        }
    }

    // ---- UI thread ---------------------------------------------------------

    /** Claim outside any gated operation; a no-op inside one. */
    private void claimAllOutside() {
        if (depth > 0) {
            return;
        }
        synchronized (LOCK) {
            depth++;
            try {
                claimAll();
            } finally {
                depth--;
            }
        }
    }

    /** {@link #LOCK} held, {@code depth > 0}: apply the pending push, then the pending reset. */
    private void claimAll() {
        if (installed != this) {
            return;
        }
        PendingPush push = pendingPush;
        pendingPush = null;
        if (push != null) {
            applyPush(push);
        }
        claimReset();
    }

    /**
     * Apply the pending reset if it is this field's; once. A reset for a field
     * older than the one the IME serves is moot and dropped; one for a newer
     * field waits for that field's first push, which establishes the owner.
     */
    private void claimReset() {
        PendingReset reset = pendingReset;
        if (reset == null) {
            return;
        }
        if (reset.owner < owner) {
            pendingReset = null;
            return;
        }
        if (reset.owner > owner) {
            return;
        }
        pendingReset = null;
        generation = reset.generation;
        setState(EMPTY);
        // setState notifies nobody: refresh GameActivity's native copy of the
        // state, then acknowledge the generation to Slint. The acknowledgement
        // carries no text; Slint's own copy is left as it is.
        activity.stateChanged(EMPTY, false);
        nativeTextInputAck(owner, reset.generation);
    }

    private void applyPush(PendingPush push) {
        if (push.owner < owner) {
            // A field the IME no longer serves: never resurrect it.
            return;
        }
        // Establish the owner first, so a reset published for this field
        // before its first push arrived can be claimed from here on. A new
        // field's first push is never stale, whatever generation the
        // previous field's Editable reflected.
        boolean newOwner = push.owner > owner;
        owner = push.owner;
        PendingReset reset = pendingReset;
        if (reset != null && reset.owner < owner) {
            // The IME moved to a newer field since that send was accepted.
            pendingReset = null;
            reset = null;
        }
        if (reset != null && reset.owner == owner && push.generation < reset.generation) {
            // Issued before the send was accepted: the reset supersedes it.
            claimReset();
            return;
        }
        if (!newOwner && push.generation < generation) {
            return;
        }
        generation = push.generation;
        setState(push.state);
        if (reset != null && reset.owner == owner) {
            // Computed after the consumption: applying it is the reset. Give
            // GameActivity's native copy the state actually applied (setState
            // notifies nobody), then acknowledge the consumption.
            pendingReset = null;
            activity.stateChanged(push.state, false);
            nativeTextInputAck(owner, reset.generation);
        }
    }

    /**
     * Run one of the IME's mutating operations after any pending request,
     * under the monitor, so its snapshot publication and the native thread's
     * next publication cannot interleave. Nested calls (setState's
     * setSelection/setComposingRegion, processKeyEvent's
     * finishComposingText/setComposingRegion/setSelection) belong to the
     * outer operation and are not gated again.
     */
    private boolean gated(Operation operation) {
        if (depth > 0) {
            return operation.run();
        }
        synchronized (LOCK) {
            depth++;
            try {
                claimAll();
                return operation.run();
            } finally {
                depth--;
            }
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
        // Edits nothing; publishes the snapshot. Gated so the publication is
        // under the monitor and a batch with no edit still reports the new
        // generation's state after a claim.
        return gated(super::endBatchEdit);
    }

    @Override
    public boolean onKey(View view, int keyCode, KeyEvent event) {
        return gated(() -> super.onKey(view, keyCode, event));
    }

    @Override
    public boolean performEditorAction(int actionCode) {
        // An action, not an edit: claim, then act on the current generation.
        claimAllOutside();
        return super.performEditorAction(actionCode);
    }

    // ---- Listener: tag every snapshot, then GameActivity's own path ----------

    @Override
    public void stateChanged(State state, boolean dismissed) {
        if (installed != this) {
            // Retired: neither copy is this gate's to report any more.
            return;
        }
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
