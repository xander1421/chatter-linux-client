//! Chatter Linux Client
//!
//! Native Linux desktop client for Chatter secure messaging.

slint::include_modules!();

fn main() {
    println!("Chatter Linux Client v0.1.0");

    let window = MainWindow::new().expect("Failed to create window");
    window.run().expect("Failed to run application");
}
