//! Read or set the mesh on/off preference without the UI.
//!
//! ```text
//! cargo run -p chatter-core --example mesh_toggle            # print it
//! cargo run -p chatter-core --example mesh_toggle -- on      # turn it on
//! cargo run -p chatter-core --example mesh_toggle -- off     # turn it off
//! ```
//!
//! The switch lives in the header of a mesh conversation, which is the right place
//! for it: the mesh exists for the moment the network fails, and a control you have
//! to go and find in a settings screen is not available then. But the preference
//! persists, and "off" is therefore reachable from states where the UI is not —
//! a headless run, an automated session on a compositor that refuses synthetic
//! clicks, a build whose window will not come up. The mesh then stays off with no
//! way back, which for a feature meant for emergencies is the wrong corner to be
//! stuck in.
//!
//! This is the way back. Deliberately an example rather than a flag in the app:
//! a `CHATTER_MESH_FORCE_ON` environment variable would be read on every start
//! forever and would quietly outrank the user's own choice, which is worse than a
//! tool you have to run on purpose.
//!
//! Reads the database the client uses, so the client should not be running — two
//! writers on one SQLCipher file is not a thing to arrange casually.

use chatter_core::storage::repositories::settings_repo::{SettingsRepository, keys};
use chatter_core::storage::Database;
use chatter_core::system::directories::Directories;
use std::sync::Arc;

fn main() {
    let wanted = match std::env::args().nth(1).as_deref() {
        None => None,
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(other) => {
            eprintln!("unrecognised argument {other:?}; expected `on`, `off`, or nothing");
            std::process::exit(2);
        }
    };

    let directories = match Directories::new() {
        Ok(dirs) => dirs,
        Err(error) => {
            eprintln!("could not locate the data directory: {error}");
            std::process::exit(1);
        }
    };
    let path = directories.database_path();

    // The same passphrase the client would use. `CHATTER_DB_PASSPHRASE` or the
    // file beside the database, so this works in exactly the setups where the UI
    // does not.
    let passphrase = std::env::var("CHATTER_DB_PASSPHRASE").ok().or_else(|| {
        std::fs::read_to_string(path.with_file_name("chatter.passphrase"))
            .ok()
            .map(|s| s.trim().to_string())
    });
    let Some(passphrase) = passphrase else {
        eprintln!(
            "no passphrase: set CHATTER_DB_PASSPHRASE, or place `chatter.passphrase` \
             beside {}",
            path.display()
        );
        std::process::exit(1);
    };

    let db = match Database::open(&path, &passphrase) {
        Ok(db) => Arc::new(db),
        Err(error) => {
            eprintln!("could not open {}: {error}", path.display());
            eprintln!("(is the client still running? it holds the database open)");
            std::process::exit(1);
        }
    };
    let settings = SettingsRepository::new(db);

    // Absent means on — the preference is opt-*out*, so a mesh nobody has switched
    // off is running. Reported as "on (default)" so the two are distinguishable.
    let current = settings.get_bool(keys::MESH_ENABLED).ok().flatten();
    let describe = |value: Option<bool>| match value {
        None => "on (default, never set)".to_string(),
        Some(true) => "on".to_string(),
        Some(false) => "off".to_string(),
    };

    match wanted {
        None => println!("mesh is {}", describe(current)),
        Some(value) => {
            if let Err(error) = settings.set_bool(keys::MESH_ENABLED, value) {
                eprintln!("could not write the setting: {error}");
                std::process::exit(1);
            }
            println!(
                "mesh was {}, now {}",
                describe(current),
                describe(Some(value))
            );
            println!("restart the client for it to take effect");
        }
    }
}
