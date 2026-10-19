//! Chatter Client - Desktop Entry Point

#[tokio::main]
async fn main() {
    // The Slint event loop runs inside this one block_on poll and never
    // returns to tokio, so the cooperative budget would never be reset: after
    // ~128 tokio operations on the UI thread (e.g. the call-orchestrator wait
    // loop's 200 ms sleeps before login), every tokio poll there yields and
    // wakes itself at once, and the event loop spins at 100% CPU.
    tokio::task::unconstrained(chatter_ui::app_main()).await;
}
