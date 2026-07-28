//! Manual smoke test for the BlueZ radio. Needs a real adapter.
//!
//! ```text
//! cargo run -p chatter-mesh-ble --example smoke
//! ```
//!
//! **This transmits.** It powers on the adapter and advertises the mesh service
//! UUID for the duration of the run, so the machine is discoverable to anything in
//! Bluetooth range. It advertises no name and no personal data, and stops on exit.
//!
//! While it runs, verify from another terminal:
//!
//! ```text
//! busctl introspect org.bluez /org/bluez/hci0    # LEAdvertisingManager1, GattManager1
//! sudo btmon                                     # service UUID present, NO local name (AD 0x08/0x09)
//! bluetoothctl devices                           # peers discovered by the scan
//! ```
//!
//! The one thing this cannot check is the central path end to end: a single adapter
//! cannot connect to itself. That needs a second machine or a phone running bitchat,
//! and it stays the open gap the P2 spec calls out.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("chatter-mesh-ble is Linux-only; nothing to smoke test here.");
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use chatter_mesh_ble::BlueZLink;
    use chatter_mesh_link::{LinkEvent, MeshLink};
    use std::time::Duration;

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);

    let link = BlueZLink::new();

    println!("starting: powering the adapter, advertising, and scanning…");
    if let Err(error) = link.start().await {
        eprintln!("start failed: {error}");
        eprintln!("if this is a permissions error, check that bluetoothd is running");
        eprintln!("and that your user may talk to org.bluez on the system bus.");
        std::process::exit(1);
    }
    // `start` deliberately does not scan: scanning is the expensive half and the
    // duty cycle owns it. Turn it on here so the smoke test exercises both roles.
    if let Err(error) = link.set_scanning(true).await {
        eprintln!("set_scanning failed: {error}");
    }
    println!("advertising and scanning. running for {seconds}s — inspect now.");

    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);

    let mut discovered = 0usize;
    let mut connected = 0usize;
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            event = link.next_event() => match event {
                None => {
                    println!("link layer shut down");
                    break;
                }
                // Worth shouting about in a smoke test: from here on the
                // adapter still advertises and still dials out, but nothing can
                // connect *to* it. A run that reports this and then looks quiet is
                // not a quiet room.
                Some(LinkEvent::PeripheralRoleEnded) => {
                    println!("  !! PERIPHERAL ROLE ENDED — inbound connections are no longer served");
                }
                Some(LinkEvent::Discovered { address, rssi, .. }) => {
                    discovered += 1;
                    println!("  discovered {address} at {rssi} dBm");
                }
                Some(LinkEvent::Connected { link: id, role, max_write }) => {
                    connected += 1;
                    // max_write is the negotiated payload ceiling, reported verbatim.
                    // If this does not change when a peer renegotiates its MTU, the
                    // fragment sizing above it is working from a stale number.
                    println!("  CONNECTED {id} role={role:?} max_write={max_write}");
                }
                Some(LinkEvent::Data { link: id, bytes }) => {
                    println!("  data on {id}: {} bytes", bytes.len());
                }
                Some(LinkEvent::Disconnected { link: id }) => {
                    println!("  disconnected {id}");
                }
                Some(LinkEvent::AdapterPowerChanged { powered }) => {
                    println!("  adapter powered={powered}");
                }
            },
        }
    }

    println!("stopping…");
    if let Err(error) = link.stop().await {
        eprintln!("stop failed: {error}");
    }
    println!("done. discovered={discovered} connected={connected}");
    if discovered == 0 {
        println!("no peers seen — expected unless something nearby advertises our service UUID.");
    }
}
