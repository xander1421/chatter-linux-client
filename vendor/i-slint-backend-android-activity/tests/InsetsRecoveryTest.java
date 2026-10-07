import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.Collections;

/** Runs the production lifecycle policy with an authoritative fake root and queued UI work. */
public final class InsetsRecoveryTest {
    static final class Root implements InsetsRecovery.Host {
        boolean attached = true, focus = true, nativeReady = true;
        int keyboard = 240, height = 936, bars = 24;
        int renderedKeyboard = -1, renderedHeight, renderedBars, requests;
        long latest;
        final ArrayDeque<Runnable> layout = new ArrayDeque<>();
        final ArrayDeque<Runnable> nativeQueue = new ArrayDeque<>();
        public boolean ready() { return attached && focus; }
        public void requestInsets() { ++requests; }
        public void postAfterLayout(Runnable action) { layout.add(action); }
        public void invalidateQueuedInsets(long seq) { latest = seq; }
        public void publishCurrent() { publish(keyboard, height, bars); }
        void publish(int ime, int h, int b) {
            if (!nativeReady) return;
            final long seq = InsetsRecovery.nextSequence();
            latest = seq;
            nativeQueue.add(() -> {
                if (seq == latest) { renderedKeyboard = ime; renderedHeight = h; renderedBars = b; }
            });
        }
        void settle() {
            while (!layout.isEmpty()) layout.remove().run();
            while (!nativeQueue.isEmpty()) nativeQueue.remove().run();
        }
    }
    static void check(boolean ok, String message) { if (!ok) throw new AssertionError(message); }
    static void recreatedOverlap(Root root, InsetsRecovery recovery, Object first, Object second) {
        recovery.prepare(first);
        recovery.prepare(second);
        int requests = root.requests;
        recovery.end(first);
        check(root.requests == requests, "overlapping animation must not settle early");
        recovery.end(second);
        root.settle();
        check(root.renderedKeyboard == root.keyboard, "last animation end pulls current IME");
    }
    public static void main(String[] args) {
        Root root = new Root();
        InsetsRecovery recovery = new InsetsRecovery(root);
        root.nativeReady = false;
        recovery.focusChanged(true);
        root.settle();
        check(root.renderedKeyboard == -1, "host attachment before native proxy must defer publication");
        root.nativeReady = true;
        recovery.regainedFocus(); // native window initialization/focus after proxy installation
        root.settle();
        check(root.renderedKeyboard == 240, "visible IME must survive recovery");
        Object interrupted = new Object();
        recovery.prepare(interrupted);
        check(recovery.progress(Arrays.asList(interrupted)), "live animation allowed");
        root.publish(240, 700, 24); // queued transient, never delivered before SAF return
        root.focus = false;
        recovery.focusChanged(false);
        root.keyboard = 0;
        root.height = 936;
        root.focus = true;
        recovery.focusChanged(true); // terminal hide callback was omitted
        check(!recovery.progress(Arrays.asList(interrupted)), "retired animation cannot overwrite return");
        recovery.end(interrupted);
        root.settle();
        check(root.renderedKeyboard == 0 && root.renderedHeight == 936 && root.renderedBars == 24,
                "hidden return must repair IME, viewport, and preserve bars");
        // Cancellation/end settles from root state, never the previous animation frame.
        Object showing = new Object();
        recovery.prepare(showing);
        root.keyboard = 180;
        check(recovery.progress(Arrays.asList(showing)), "new animation allowed");
        root.publish(90, 936, 24);
        recovery.end(showing);
        check(!recovery.progress(Arrays.asList(showing)), "late finished frame is fenced");
        root.settle();
        check(root.renderedKeyboard == 180, "animation end preserves genuinely visible IME");
        Object barsAnimation = new Object();
        Object imeAnimation = new Object();
        recreatedOverlap(root, recovery, barsAnimation, imeAnimation);
        Object missingJavaFocus = new Object();
        recovery.prepare(missingJavaFocus);
        root.keyboard = 0;
        recovery.regainedFocus(); // Native GainedFocus repairs even if Java lost-focus/onEnd were omitted.
        check(!recovery.progress(Arrays.asList(missingJavaFocus)), "native return retires interrupted animation");
        root.settle();
        check(root.renderedKeyboard == 0, "native focus recovery must not remain blocked by old animation");
        root.keyboard = 180;
        recovery.reconcile();
        root.settle();
        root.height = 800;
        root.bars = 32;
        recovery.reconcile();
        root.settle();
        check(root.renderedHeight == 800 && root.renderedBars == 32, "resize uses new geometry");
        // A posted recovery belonging to the detached view must not replay into its successor.
        root.keyboard = 0;
        recovery.reconcile();
        recovery.detached();
        root.attached = false;
        root.settle();
        check(root.renderedKeyboard == 180, "detach fences pending native and posted publications");
        root.attached = true;
        root.focus = true;
        InsetsRecovery recreated = new InsetsRecovery(root);
        recreated.returnToWindow();
        recovery.detached(); // old helper teardown must not invalidate the successor's queued snapshot
        root.settle();
        check(root.renderedKeyboard == 0 && root.requests > 0, "recreated focused root recovers");
        check(!recreated.progress(Collections.emptyList()), "unowned progress is rejected");
        root.attached = false;
        recreated.regainedFocus();
        root.settle();
        root.attached = true;
        root.keyboard = 160;
        recreated.returnToWindow(); // the later attached/layout boundary must finish recovery
        root.settle();
        check(root.renderedKeyboard == 160, "attachment/layout after early focus must recover visible IME");
        System.out.println("InsetsRecovery: hidden/visible return, interrupted/late animation, resize, detach/recreation passed");
    }
}
