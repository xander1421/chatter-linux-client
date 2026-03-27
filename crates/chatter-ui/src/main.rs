//! Chatter Client - Desktop Entry Point

#[tokio::main]
async fn main() {
    chatter_ui::app_main().await;
}
