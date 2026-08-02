//! VALIDATED-UI render smoke — "is anything actually ON SCREEN".
//!
//! The gap this closes, stated plainly: `ui_wiring.rs` once passed **43/43
//! while the app rendered a completely blank screen**. A chat column had been
//! removed by a collapse-state mismatch, and not one of those tests could tell
//! — they prove a callback reaches core, which is true of a callback nobody can
//! see. The regression was caught by a human looking at a screenshot.
//!
//! So this file asserts the one thing the wiring suite structurally cannot: the
//! main view has real geometry, its expected children EXIST, and they COVER the
//! window. Not pixels — no baselines to rot — just "there is something there,
//! and it is the right size".
//!
//! The load-bearing conditional is `ui/app.slint`:
//!
//! ```slint
//! if !(root.is-mobile-platform && !root.sidebar-is-rail): chat-panel-area := ...
//! ```
//!
//! with `sidebar-is-rail: root.is-small-screen || root.sidebar-collapsed`. Get
//! the polarity of either term wrong and the WHOLE right-hand side of the app —
//! chat, composer, thread panel — vanishes, while every wiring test stays
//! green because the callbacks are still registered on a window nobody renders.
//!
//! Own test BINARY, and exactly one `#[test]`: the integration backend is
//! `set_platform`-once-per-process (see the upstream doc on
//! `init_integration_test_with_system_time`), and `ui_layout.rs` already claims
//! it in its own process.

use chatter_ui::MainWindow;
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

/// Fail with the cause, not the symptom, when the element-query API is inert.
///
/// Without `SLINT_EMIT_DEBUG_INFO=1` every query returns nothing, and a
/// render-smoke test then reports a totally blank UI — the exact thing it is
/// hunting — when the only thing wrong is the command line.
fn require_element_queries() {
    assert_eq!(
        env!("CHATTER_UI_SLINT_DEBUG_INFO"),
        "1",
        "the ElementHandle query API is inert: chatter-ui was built without \
         SLINT_EMIT_DEBUG_INFO=1, so every element query returns nothing and \
         this suite would report a blank screen no matter what renders. Run:\n\n    \
         ./scripts/test-ui.sh"
    );
}

/// Wall-clock a pump must cover. The longest animation on anything asserted
/// below is `ui/sidebar/sidebar.slint:158` — `animate width { duration: 200ms }`.
/// Measure before it settles and the mobile sidebar reads 101px on its way from
/// 68px to 800px, which looks exactly like the blank-screen bug this file hunts.
const SETTLE_MS: u64 = 500;
const FRAME_MS: u64 = 20;

/// Force a real layout pass, let conditional sub-trees materialize, and let
/// animations run to completion.
///
/// Frames are driven one at a time with a real sleep between them, rather than
/// `ui_layout.rs`'s single long `w.run()`. That file only pumps a freshly-built
/// window, where one entry is enough; here we pump repeatedly to compare
/// states, and the event loop cannot be re-entered that way: after the first
/// `quit_event_loop()`, `w.run()` returns in ~0.3ms instead of honouring its
/// timer (measured). Animations are driven by the SYSTEM clock under
/// `init_integration_test_with_system_time`, so what actually advances them is
/// wall time passing between frames.
fn pump(w: &MainWindow) {
    for _ in 0..(SETTLE_MS / FRAME_MS) {
        let scale = w.window().scale_factor();
        let size = w.window().size().to_logical(scale);
        w.window().dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(size.width, size.height),
        });
        slint::Timer::single_shot(std::time::Duration::from_millis(1), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(FRAME_MS));
    }
}

struct Box_ {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Geometry of the FIRST element of `ty`, or None if it is not in the tree.
fn find(w: &MainWindow, ty: &str) -> Option<Box_> {
    let e = testing::ElementHandle::find_by_element_type_name(w, ty).next()?;
    let p = e.absolute_position();
    let s = e.size();
    Some(Box_ { x: p.x, y: p.y, w: s.width, h: s.height })
}

fn count(w: &MainWindow, ty: &str) -> usize {
    testing::ElementHandle::find_by_element_type_name(w, ty).count()
}

/// The whole point: an element that exists but is 0x0 is as invisible as one
/// that was never instantiated, and both are "blank screen" to a user.
fn assert_visible(b: &Option<Box_>, ty: &str, ctx: &str) -> &'static () {
    let b = b.as_ref().unwrap_or_else(|| {
        panic!("BLANK SCREEN [{ctx}]: `{ty}` is not in the element tree at all")
    });
    assert!(
        b.w > 1.0 && b.h > 1.0,
        "BLANK SCREEN [{ctx}]: `{ty}` is in the tree but laid out {}x{} at \
         ({}, {}) — an element with no area renders nothing",
        b.w,
        b.h,
        b.x,
        b.y
    );
    &()
}

#[test]
fn main_view_renders_its_columns_and_they_cover_the_window() {
    require_element_queries();
    testing::init_integration_test_with_system_time();

    let w = MainWindow::new().expect("MainWindow::new");
    w.set_current_view("main".into());
    // Default desktop shape: a real window, mouse input, sidebar expanded.
    w.set_is_mobile_platform(false);
    w.set_sidebar_collapsed(false);
    pump(&w);

    let win = w.window().size().to_logical(w.window().scale_factor());
    let (win_w, win_h) = (win.width, win.height);

    // ---- 0. The window itself has area ------------------------------------
    // If this fails nothing below can mean anything: every child would be
    // trivially "inside" a zero-sized window.
    assert!(
        win_w > 100.0 && win_h > 100.0,
        "BLANK SCREEN: the window itself is {win_w}x{win_h}"
    );

    // ---- 1. Desktop main view is not blank --------------------------------
    let sidebar = find(&w, "Sidebar");
    let chat = find(&w, "ChatLayout");
    assert_visible(&sidebar, "Sidebar", "desktop main view");
    assert_visible(&chat, "ChatLayout", "desktop main view");
    let sidebar = sidebar.unwrap();
    let chat = chat.unwrap();
    println!(
        "desktop: window={win_w}x{win_h} sidebar={}..{} chat={}..{}",
        sidebar.x,
        sidebar.x + sidebar.w,
        chat.x,
        chat.x + chat.w
    );

    // ---- 2. …and the two columns TILE the window --------------------------
    // The 2026-07-19 thread-panel bug and the blank-screen regression are the
    // same shape: an element present in the tree, sized, and sitting outside
    // the part of the window a user can see. Covering the width is the cheapest
    // honest proof that the main view is not mostly void.
    assert!(
        sidebar.x >= -0.5,
        "Sidebar starts off-window at x={}",
        sidebar.x
    );
    assert!(
        chat.x >= sidebar.x + sidebar.w - 0.5,
        "chat column overlaps the sidebar: sidebar ends at {}, chat starts at {}",
        sidebar.x + sidebar.w,
        chat.x
    );
    assert!(
        (chat.x + chat.w) >= win_w - 1.0,
        "BLANK SCREEN: {:.0}px of window to the RIGHT of the chat column is \
         empty (chat ends at {}, window is {win_w} wide)",
        win_w - (chat.x + chat.w),
        chat.x + chat.w
    );
    assert!(
        chat.h >= win_h * 0.5,
        "chat column is only {}px tall in a {win_h}px window",
        chat.h
    );

    // ---- 3. The chat column is not an empty shell -------------------------
    // A ChatLayout with no composer is a conversation you cannot answer. This
    // also proves the query layer reaches DEEP descendants under the
    // integration backend, which the headless `init_no_event_loop` one used by
    // ui_wiring.rs does not.
    assert!(
        count(&w, "MessageComposer") > 0,
        "BLANK SCREEN: ChatLayout rendered with no MessageComposer in it"
    );

    // ---- 4. Mobile, sidebar as a rail: the chat column SURVIVES -----------
    // `!(is-mobile-platform && !sidebar-is-rail)`. This is the exact term whose
    // polarity, flipped, blanks the app on Android.
    w.set_is_mobile_platform(true);
    w.set_sidebar_collapsed(true);
    pump(&w);
    let m_side = find(&w, "Sidebar");
    let m_chat = find(&w, "ChatLayout");
    assert_visible(&m_side, "Sidebar", "mobile, sidebar collapsed to a rail");
    assert_visible(&m_chat, "ChatLayout", "mobile, sidebar collapsed to a rail");
    let m_side = m_side.unwrap();
    let m_chat = m_chat.unwrap();
    println!(
        "mobile rail: sidebar={}..{} chat={}..{}",
        m_side.x,
        m_side.x + m_side.w,
        m_chat.x,
        m_chat.x + m_chat.w
    );
    assert!(
        (m_chat.x + m_chat.w) >= win_w - 1.0,
        "BLANK SCREEN on mobile: chat column ends at {} in a {win_w}px window",
        m_chat.x + m_chat.w
    );
    assert!(
        m_side.w < win_w * 0.5,
        "the collapsed rail is {}px wide in a {win_w}px window — it is not a rail",
        m_side.w
    );

    // ---- 5. Mobile, sidebar expanded: it must fill the window -------------
    // Here the chat column is REMOVED on purpose (the panel takes the screen).
    // That is only correct if the sidebar then covers the width; a half-width
    // sidebar beside a removed chat column is a literal blank half-screen, and
    // it is the shape the original regression shipped in.
    w.set_sidebar_collapsed(false);
    pump(&w);
    let x_side = find(&w, "Sidebar");
    assert_visible(&x_side, "Sidebar", "mobile, sidebar expanded");
    let x_side = x_side.unwrap();
    println!(
        "mobile expanded: sidebar={}..{} chatlayouts={}",
        x_side.x,
        x_side.x + x_side.w,
        count(&w, "ChatLayout")
    );
    assert!(
        (x_side.x + x_side.w) >= win_w - 1.0 && x_side.x <= 0.5,
        "BLANK SCREEN on mobile: the chat column is removed while the sidebar \
         only covers {}..{} of a {win_w}px window — the rest is void",
        x_side.x,
        x_side.x + x_side.w
    );
    assert!(
        x_side.h >= win_h * 0.5,
        "mobile sidebar is only {}px tall in a {win_h}px window",
        x_side.h
    );
}
