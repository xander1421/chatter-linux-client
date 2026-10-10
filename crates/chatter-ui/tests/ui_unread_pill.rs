//! Where the message list is, and where it goes: the "N new messages" pill,
//! jumps to a row, following the newest message, older history at the top.
//! Own test binary, one test: the integration testing backend is
//! once-per-process.
//!
//! The list is virtualized, so there is no pixel position to assert on — the
//! runtime estimates one from the rows on screen. `message-scroll-position` is
//! that estimate; with rows of one height it is `row × height`, which the
//! tests measure (`row_height`) rather than assume.

use std::cell::Cell;
use std::rc::Rc;

use chatter_ui::{MainWindow, MessageItemData};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn pump(w: &MainWindow) {
    // Opening on an unread row and jumping are driven by Slint `Timer`
    // elements (1fc72cab, d33a9743) that measure the target row's delegate
    // after the list moved. This harness never renders, so do what a real
    // frame does between two timer ticks: fire due timers, run change
    // handlers, then lay out. Without the layout pass a timer re-measured the
    // delegate's position from before the list moved, read it as converged,
    // and a jump to row 50 stopped on row 35.
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(16));
        slint::platform::update_timers_and_animations();
        i_slint_core::properties::ChangeTracker::run_change_handlers();
        layout_pass(w);
    }
    slint::Timer::single_shot(std::time::Duration::from_millis(400), || {
        slint::quit_event_loop().unwrap();
    });
    w.run().unwrap();
}

/// The testing backend never draws, and the message list lays its rows out
/// only inside the window's instantiation pass (what a frame, an input event
/// or an element query triggers). A pointer move is that pass: it loops
/// instantiation and change handlers until nothing moves, which is the same
/// loop a real frame runs before it is drawn. The pointer has to actually
/// move: an event repeating the last position triggers nothing.
fn layout_pass(w: &MainWindow) {
    thread_local!(static X: Cell<f32> = const { Cell::new(1.0) });
    let x = X.with(|x| x.replace(3.0 - x.get()));
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, 1.0),
        });
}

/// One layout pass, then frames until the list comes to rest.
fn lay_out(w: &MainWindow) {
    layout_pass(w);
    pump(w);
}

/// A scroll by the user, `dy` > 0 towards older messages. Laid out once it
/// has come to rest, as the frames of a real scroll would have.
fn wheel(w: &MainWindow, dy: f32) {
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(400.0, 300.0),
            delta_x: 0.0,
            delta_y: dy,
        });
    pump(w);
    lay_out(w);
}

fn row(i: usize) -> MessageItemData {
    MessageItemData {
        message_id: format!("m{i}").into(),
        sender_id: "dev".into(),
        sender_name: "Dev".into(),
        content: format!("message {i}").into(),
        timestamp_ns: format!("{i}").into(),
        show_sender: true,
        ..Default::default()
    }
}

fn edit(w: &MainWindow, f: impl FnOnce(&VecModel<MessageItemData>)) {
    let model = w.get_messages();
    f(model
        .as_any()
        .downcast_ref::<VecModel<MessageItemData>>()
        .expect("VecModel"));
}

/// Open a conversation the way `sidebar::channel` does: loading raised, then
/// anchor + page + loading lowered in one UI closure. `unread_index` < 0 opens
/// on the newest message.
fn open(w: &MainWindow, rows: std::ops::Range<usize>, unread_index: i32) {
    w.set_loading_messages(true);
    pump(w);
    w.set_unread_index(unread_index);
    w.set_unread_count(if unread_index < 0 { 0 } else { 1 });
    w.set_is_at_bottom(unread_index < 0);
    w.set_messages(ModelRc::new(VecModel::from(
        rows.map(row).collect::<Vec<_>>(),
    )));
    w.set_loading_messages(false);
    lay_out(w);
}

/// What `header::jump_to_message` does to the list.
fn jump(w: &MainWindow, index: i32) {
    w.set_scroll_to_message_index(index);
    lay_out(w);
    w.set_scroll_to_message_index(-1);
    lay_out(w);
}

fn pos(w: &MainWindow) -> f32 {
    w.get_message_scroll_position()
}

/// Row at the top edge, given the height of one.
fn top_row(w: &MainWindow, row_height: f32) -> f32 {
    pos(w) / row_height
}

#[test]
fn message_list_navigation() {
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");
    w.set_current_view("main".into());
    // The chat panel exists only for an open conversation (app.slint gates
    // ChatLayout on a non-empty channel name); without one, no list is ever
    // created and nothing here is tested.
    w.set_channel_name("dev".into());
    w.show().unwrap();
    pump(&w);

    pill_only_when_unread_is_below_the_viewport(&w);
    let row_height = jumps_land_on_their_row(&w);
    new_messages_are_followed_only_from_the_bottom(&w);
    scrolling_does_not_read_row_zero(&w);
    older_history_loads_to_the_beginning(&w, row_height);
}

/// The message rows, counting how often row 0 is copied out.
struct RowZeroReads {
    rows: VecModel<MessageItemData>,
    reads: Cell<usize>,
}

impl Model for RowZeroReads {
    type Data = MessageItemData;
    fn row_count(&self) -> usize {
        self.rows.row_count()
    }
    fn row_data(&self, row: usize) -> Option<MessageItemData> {
        if row == 0 {
            self.reads.set(self.reads.get() + 1);
        }
        self.rows.row_data(row)
    }
    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        self.rows.model_tracker()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// PERF: the list's per-layout-pass bookkeeping (`settle`) runs on every
/// scroll frame. Row 0 can only become a different row when the row count
/// changes, so a scroll that leaves the count alone must not copy it out of
/// the model — a whole MessageItemData per frame.
fn scrolling_does_not_read_row_zero(w: &MainWindow) {
    let model = Rc::new(RowZeroReads {
        rows: VecModel::from((0..300).map(row).collect::<Vec<_>>()),
        reads: Cell::new(0),
    });
    w.set_loading_messages(true);
    pump(w);
    w.set_unread_index(-1);
    w.set_unread_count(0);
    w.set_is_at_bottom(true);
    w.set_messages(ModelRc::from(model.clone()));
    w.set_loading_messages(false);
    lay_out(w);
    assert!(
        model.reads.get() > 0,
        "the open never recorded row 0: the counter is not wired"
    );

    // Far from the top throughout: row 0 is never live, nor the row under it
    // (whose date separator compares against row 0).
    wheel(w, 600.0);
    let (reads, start) = (model.reads.get(), pos(w));
    for _ in 0..5 {
        wheel(w, 300.0);
    }
    assert!(
        start - pos(w) > 500.0,
        "the wheel did not scroll: {start} → {}",
        pos(w)
    );
    assert_eq!(
        model.reads.get(),
        reads,
        "scrolling copied row 0 out of the model on its layout passes"
    );
}

/// Regression (device sweep, client 8adfb834): a conversation opened on an
/// unread boundary whose whole tail fits on screen still showed "1 new message ↓"
/// over the message itself. The open clears `is-at-bottom` for the anchor and
/// the list never re-derived it, because nothing it listened to *changed* after
/// the freshly created Flickable laid out.
fn pill_only_when_unread_is_below_the_viewport(w: &MainWindow) {
    // Three short bubbles, the last unread: everything is on screen.
    open(w, 0..3, 2);
    assert!(
        w.get_is_at_bottom(),
        "short conversation opened on an unread boundary still reports it is \
         scrolled away from the bottom — the \"1 new message\" pill floats over \
         the message it points at"
    );
    assert_eq!(
        w.get_unread_count(),
        0,
        "caught up, so the pill has no count"
    );

    // Control: a long conversation opened on an early boundary really does
    // have unread messages below the viewport.
    open(w, 0..200, 5);
    assert!(
        !w.get_is_at_bottom(),
        "long conversation anchored near the top claims to be at the bottom"
    );
}

/// Regression (device sweep RC25): a quote tap did nothing or moved a row, a
/// search result opened its conversation at the end. Sitting on the newest
/// message, the list took the re-estimate its own jump caused for content
/// growing and went back to the end; and a jump recorded before the list
/// existed was never seen by it.
fn jumps_land_on_their_row(w: &MainWindow) -> f32 {
    open(w, 0..200, -1);
    assert!(
        w.get_is_at_bottom(),
        "a conversation with nothing unread opens on the newest message"
    );
    let end = pos(w);

    jump(w, 100);
    let at_100 = pos(w);
    jump(w, 50);
    let at_50 = pos(w);
    assert!(
        at_100 < end - 1000.0 && at_50 < at_100 - 1000.0,
        "jumps from the newest message did not leave it: end {end}, row 100 at \
         {at_100}, row 50 at {at_50}"
    );
    assert!(
        !w.get_is_at_bottom(),
        "a jump into history is not the bottom"
    );
    let row_height = (at_100 - at_50) / 50.0;
    assert!(
        (top_row(w, row_height) - 50.0).abs() <= 1.0,
        "row 50 is {} rows off",
        top_row(w, row_height) - 50.0
    );

    // A neighbour: the runtime walks there instead of seeking.
    jump(w, 47);
    assert!(
        (top_row(w, row_height) - 47.0).abs() <= 1.0,
        "a jump three rows up landed on row {}",
        top_row(w, row_height)
    );

    // The same row again, after scrolling away from it.
    wheel(w, -600.0);
    assert!(
        (top_row(w, row_height) - 47.0).abs() > 2.0,
        "the wheel did not scroll"
    );
    jump(w, 47);
    assert!(
        (top_row(w, row_height) - 47.0).abs() <= 1.0,
        "a second jump to the same row landed on row {}",
        top_row(w, row_height)
    );

    // A search result: the conversation opens and the jump is recorded in the
    // same turn, before the list exists.
    w.set_loading_messages(true);
    pump(w);
    w.set_unread_index(-1);
    w.set_is_at_bottom(true);
    w.set_messages(ModelRc::new(VecModel::from(
        (0..200).map(row).collect::<Vec<_>>(),
    )));
    w.set_loading_messages(false);
    w.set_scroll_to_message_index(120);
    lay_out(w);
    assert!(
        (top_row(w, row_height) - 120.0).abs() <= 1.0,
        "a conversation opened for a search result is on row {}, not the result",
        top_row(w, row_height)
    );
    w.set_scroll_to_message_index(-1);
    row_height
}

/// Regression (device sweep RC25): a message arriving while the newest one was
/// on screen was left below the fold, the sender's own included. The list read
/// the longer content as "no longer at the bottom" before it looked at why.
fn new_messages_are_followed_only_from_the_bottom(w: &MainWindow) {
    open(w, 0..200, -1);
    let before = pos(w);
    edit(w, |m| m.push(row(200)));
    lay_out(w);
    assert!(
        w.get_is_at_bottom(),
        "a message arriving at the bottom took the list off it"
    );
    assert!(
        pos(w) > before + 20.0,
        "the list did not move down for the new message: {before} → {}",
        pos(w)
    );

    // Several in one turn, as a burst arrives.
    let before = pos(w);
    edit(w, |m| (201..216).for_each(|i| m.push(row(i))));
    lay_out(w);
    assert!(
        w.get_is_at_bottom() && pos(w) > before + 300.0,
        "a burst was not followed"
    );

    // Scrolled up, the reader is left where they are.
    wheel(w, 600.0);
    wheel(w, 600.0);
    assert!(
        !w.get_is_at_bottom(),
        "scrolled two screens up and still \"at the bottom\""
    );
    let before = pos(w);
    edit(w, |m| m.push(row(216)));
    lay_out(w);
    assert!(
        !w.get_is_at_bottom(),
        "a message arriving claimed the bottom for a reader in history"
    );
    assert!(
        (pos(w) - before).abs() < 2.0,
        "a reader in history was moved by an arriving message: {before} → {}",
        pos(w)
    );

    // Their own message does take them there (`append_message_to_ui`).
    w.set_is_at_bottom(true);
    edit(w, |m| m.push(row(217)));
    lay_out(w);
    assert!(
        w.get_is_at_bottom() && pos(w) > before + 600.0,
        "own send did not scroll to the newest message"
    );
}

/// Regression (device sweep RC25): after a cold start nothing older than the
/// first page ever loaded. The request fired once, and the list asked again
/// only when the model grew — which a request that added nothing never did.
fn older_history_loads_to_the_beginning(w: &MainWindow, row_height: f32) {
    // History 0..200, the newest 50 loaded; each request adds the 50 before
    // the oldest loaded row, on a later turn as `header::load_older` does —
    // alternately at once (the page is in before the reader reaches the top)
    // and late (the reader is waiting at the top, row 0 on screen).
    let requests = Rc::new(Cell::new(0));
    let weak = w.as_weak();
    let counted = requests.clone();
    w.on_load_more(move || {
        counted.set(counted.get() + 1);
        let weak = weak.clone();
        let delay = std::time::Duration::from_millis(20 * (counted.get() % 2));
        slint::Timer::single_shot(delay, move || {
            let w = weak.unwrap();
            let oldest: usize = w.get_messages().row_data(0).unwrap().message_id[1..]
                .parse()
                .unwrap();
            edit(&w, |m| {
                for (at, i) in (oldest.saturating_sub(50)..oldest).enumerate() {
                    m.insert(at, row(i));
                }
            });
        });
    });

    open(w, 150..200, -1);
    assert_eq!(
        requests.get(),
        0,
        "opening on the newest message asked for history"
    );

    for _ in 0..300 {
        let (rows, before) = (w.get_messages().row_count(), pos(w));
        if rows == 200 && before < 1.0 {
            break;
        }
        wheel(w, 500.0);
        // A page lands while the loop is pumped, which is after that turn's
        // layout pass: lay out again until one has seen every row.
        loop {
            let seen = w.get_messages().row_count();
            lay_out(w);
            if w.get_messages().row_count() == seen {
                break;
            }
        }
        let added = w.get_messages().row_count() - rows;
        if added > 0 {
            // What was on screen stays there: the position moves by the
            // scroll and by exactly the rows inserted above.
            let moved = (pos(w) - before) / row_height - added as f32;
            assert!(
                moved.abs() <= 8.0,
                "loading {added} older rows moved the content by {moved} rows"
            );
        }
    }
    assert_eq!(
        w.get_messages().row_count(),
        200,
        "history stopped loading after {} requests",
        requests.get()
    );
    assert!(
        pos(w) < 1.0,
        "the beginning of history is not reachable: stopped at {}",
        pos(w)
    );
    assert_eq!(w.get_messages().row_data(0).unwrap().message_id, "m0");
    assert!(
        requests.get() <= 5,
        "{} requests for 3 pages: the list asks more than once per approach to the top",
        requests.get()
    );
}
