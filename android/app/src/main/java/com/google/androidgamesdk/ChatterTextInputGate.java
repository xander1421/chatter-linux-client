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
 * This subclass is the one InputConnection the IME talks to (installed over
 * the surface view's own; see {@link #install}). It is the point where the
 * two copies are ordered:
 *
 * <ul>
 * <li>Native accepts a send: {@link #publishReset} stores an immutable pending
 *     reset {@code (owner, generation)} at once, from any thread, without
 *     waiting for the UI thread.</li>
 * <li>Every mutating IME operation first {@link #claim}s a pending reset on
 *     the UI thread: the Editable is emptied (and input restarted) before the
 *     operation runs, so the operation applies to the new generation. A posted
 *     fallback claims it too if no operation comes; a claim happens once.</li>
 * <li>Every snapshot the IME produces is tagged with the owner and the
 *     generation applied when it was produced ({@link #stateChanged}), so the
 *     native side drops snapshots from before the reset by provenance, never
 *     by content.</li>
 * <li>Every outbound Slint state ({@link #push}) is versioned: one issued
 *     before the accepted send (an older generation) is dropped on the UI
 *     thread instead of refilling the Editable after the reset.</li>
 * </ul>
 *
 * In this package so it can replace {@code InputEnabledSurfaceView.mInputConnection}
 * (package-private) the way the view's own constructor filled it. GameActivity's
 * native handle keeps the original object; everything native calls on it
 * (show/hide IME, restartInput) acts on the target view, not on that
 * object's Editable, and every state push now comes through here.
 */
public final class ChatterTextInputGate extends InputConnection implements Listener {
    private static final String TAG = "chatter.TextInputGate";

    /** An accepted send's consumption of the field {@code owner}, immutable. */
    private static final class PendingReset {
        final long owner;
        final long generation;

        PendingReset(long owner, long generation) {
            this.owner = owner;
            this.generation = generation;
        }
    }

    private static final State EMPTY = new State("", 0, 0, -1, -1);
    private static final AtomicReference<PendingReset> PENDING = new AtomicReference<>();
    private static final Handler MAIN = new Handler(Looper.getMainLooper());
    private static volatile ChatterTextInputGate installed;

    private final GameActivity activity;
    /** UI thread only: the field the IME serves, as native numbers it (last push). */
    private long owner;
    /** UI thread only: the generation the Editable reflects (last reset or push). */
    private long generation;

    private ChatterTextInputGate(GameActivity activity, View view, Settings settings) {
        super(activity, view, settings);
        this.activity = activity;
    }

    /**
     * Replace the surface view's InputConnection with the gate. Call from
     * {@code Activity.onCreate} after {@code super.onCreate}: the view and its
     * connection exist, the IME has not attached yet.
     */
    public static void install(GameActivity activity) {
        GameActivity.InputEnabledSurfaceView view = activity.mSurfaceView;
        if (view == null) {
            return;
        }
        InputConnection previous = view.mInputConnection;
        EditorInfo info = previous != null ? previous.getEditorInfo() : activity.getImeEditorInfo();
        // The settings the view's constructor derives: key events are
        // forwarded for an untyped field (GameActivity.java, InputEnabledSurfaceView).
        ChatterTextInputGate gate =
                new ChatterTextInputGate(activity, view, new Settings(info, info.inputType == 0));
        gate.setListener(gate);
        view.mInputConnection = gate;
        installed = gate;
    }

    // ---- Native side (any thread) ------------------------------------------

    /** Native accepted a send of the field {@code owner}: its Editable is consumed as of now. */
    public static void publishReset(long owner, long generation) {
        PENDING.set(new PendingReset(owner, generation));
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

    /** Apply a pending reset for the field the IME serves; once. */
    private void claim() {
        PendingReset reset = PENDING.get();
        if (reset == null || reset.owner != owner) {
            // Another field's reset: a push for that field resolves it (applyPush).
            return;
        }
        if (!PENDING.compareAndSet(reset, null)) {
            return;
        }
        generation = reset.generation;
        setState(EMPTY);
    }

    private void applyPush(
            String text, int selStart, int selEnd, int compStart, int compEnd, long pushOwner, long pushGeneration) {
        PendingReset reset = PENDING.get();
        if (reset != null) {
            if (reset.owner != pushOwner) {
                // The IME moved to another field since the send was accepted:
                // the consumed field's reset is moot, this push defines the new field.
                PENDING.compareAndSet(reset, null);
            } else if (pushGeneration < reset.generation) {
                // Issued before the send was accepted: the reset supersedes it.
                claim();
                return;
            } else {
                // Already reflects the consumption.
                PENDING.compareAndSet(reset, null);
            }
        }
        if (pushOwner == owner && pushGeneration < generation) {
            return;
        }
        owner = pushOwner;
        generation = pushGeneration;
        setState(new State(text, selStart, selEnd, compStart, compEnd));
    }

    // ---- The IME's operations: claim first ----------------------------------

    @Override
    public boolean setComposingText(CharSequence text, int newCursorPosition) {
        claim();
        return super.setComposingText(text, newCursorPosition);
    }

    @Override
    public boolean setComposingRegion(int start, int end) {
        claim();
        return super.setComposingRegion(start, end);
    }

    @Override
    public boolean finishComposingText() {
        claim();
        return super.finishComposingText();
    }

    @Override
    public boolean commitText(CharSequence text, int newCursorPosition) {
        claim();
        return super.commitText(text, newCursorPosition);
    }

    @Override
    public boolean commitCompletion(CompletionInfo text) {
        claim();
        return super.commitCompletion(text);
    }

    @Override
    public boolean commitCorrection(CorrectionInfo correctionInfo) {
        claim();
        return super.commitCorrection(correctionInfo);
    }

    @Override
    public boolean deleteSurroundingText(int beforeLength, int afterLength) {
        claim();
        return super.deleteSurroundingText(beforeLength, afterLength);
    }

    @Override
    public boolean deleteSurroundingTextInCodePoints(int beforeLength, int afterLength) {
        claim();
        return super.deleteSurroundingTextInCodePoints(beforeLength, afterLength);
    }

    @Override
    public boolean setSelection(int start, int end) {
        claim();
        return super.setSelection(start, end);
    }

    @Override
    public boolean sendKeyEvent(KeyEvent event) {
        claim();
        return super.sendKeyEvent(event);
    }

    @Override
    public boolean performEditorAction(int actionCode) {
        claim();
        return super.performEditorAction(actionCode);
    }

    @Override
    public boolean endBatchEdit() {
        // A batch with no edit still reports a snapshot: make it the new generation's.
        claim();
        return super.endBatchEdit();
    }

    @Override
    public boolean onKey(View view, int keyCode, KeyEvent event) {
        claim();
        return super.onKey(view, keyCode, event);
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
}
