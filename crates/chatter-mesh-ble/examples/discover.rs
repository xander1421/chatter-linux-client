//! Raw discovery dump, for bringing up a second device.
//!
//! ```text
//! cargo run -p chatter-mesh-ble --example discover [seconds]
//! ```
//!
//! `smoke` answers "does our stack advertise and scan"; this answers the
//! question that comes next and is much harder to guess at: **when a peer is on
//! air and we still report nothing, which check dropped it?**
//!
//! It runs the same discovery filter the mesh uses and prints every device the
//! stream reports, with the two properties the mesh gates on — the advertised
//! service UUIDs and the RSSI — so a peer that is visible to `bluetoothctl` but
//! invisible to the mesh shows exactly where it is lost. Scans only; it does not
//! advertise and does not connect.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("chatter-mesh-ble is Linux-only; nothing to discover here.");
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use bluer::{AdapterEvent, DiscoveryFilter, DiscoveryTransport};
    use chatter_mesh_link::SERVICE_UUID;
    use futures_util::StreamExt;
    use std::collections::HashSet;
    use std::time::Duration;
    use uuid::Uuid;

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(30);

    let service: Uuid = SERVICE_UUID.parse().expect("the mesh service UUID is a UUID");
    println!("mesh service UUID: {service}");

    let session = bluer::Session::new().await.expect("bluez session");
    let adapter = session.default_adapter().await.expect("default adapter");
    adapter.set_powered(true).await.expect("power on");
    println!("adapter: {}", adapter.name());

    // The same filter the mesh installs, except that the UUID list is left empty
    // on purpose: this tool has to be able to see a peer the mesh would filter
    // out, or it cannot tell you that the filter is what dropped it.
    //
    // MESH_FILTER=1 puts the mesh's own UUID list back, which is how you tell a
    // peer that BlueZ refuses to report from one our own code drops.
    let use_uuid_filter = std::env::var("MESH_FILTER").is_ok_and(|v| v == "1");
    let uuids = if use_uuid_filter {
        println!("discovery filter: UUIDs=[{service}] (the mesh's own filter)");
        [service].into_iter().collect()
    } else {
        println!("discovery filter: no UUID list (sees everything)");
        Default::default()
    };
    adapter
        .set_discovery_filter(DiscoveryFilter {
            uuids,
            transport: DiscoveryTransport::Le,
            rssi: Some(-127),
            duplicate_data: true,
            ..Default::default()
        })
        .await
        .expect("set discovery filter");

    let discovery = adapter
        .discover_devices_with_changes()
        .await
        .expect("start discovery");
    let mut discovery = Box::pin(discovery);

    println!("scanning for {seconds}s — every device, and why the mesh would keep or drop it\n");

    let mut seen: HashSet<String> = HashSet::new();
    let mut matched = 0usize;
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = discovery.next() => {
                let Some(event) = event else { break };
                let AdapterEvent::DeviceAdded(address) = event else { continue };
                let Ok(device) = adapter.device(address) else { continue };

                let uuids = device.uuids().await.ok().flatten();
                let rssi = device.rssi().await.ok().flatten();
                let name = device.name().await.ok().flatten().unwrap_or_default();

                let has_service = uuids.as_ref().is_some_and(|u| u.contains(&service));
                // Exactly the two gates in `central::start_scan`.
                let verdict = match (has_service, rssi) {
                    (true, Some(_)) => { matched += 1; "KEPT" }
                    (true, None) => "DROPPED: our service, but BlueZ reports no RSSI yet",
                    (false, _) if uuids.is_none() => "dropped: no UUIDs property at all",
                    (false, _) => "dropped: not advertising our service",
                };

                // One line per device per state change would drown the output;
                // a device is reported when it is new or when its verdict is
                // newsworthy.
                let key = format!("{address}|{verdict}");
                if !seen.insert(key) {
                    continue;
                }

                println!(
                    "{address}  rssi={:<5} {:<38} uuids={} {}",
                    rssi.map(|r| r.to_string()).unwrap_or_else(|| "-".into()),
                    verdict,
                    uuids.as_ref().map_or(0, HashSet::len),
                    if name.is_empty() { String::new() } else { format!("name={name}") },
                );
                if has_service {
                    println!("    ^ mesh peer. uuids: {uuids:?}");
                }
            }
        }
    }

    println!("\ndone. devices seen={} mesh peers kept={matched}", seen.len());
    if matched == 0 {
        println!(
            "no mesh peer was kept. If a device above says DROPPED with our service UUID, the \
             RSSI gate in central::start_scan is what loses it."
        );
    }
}
