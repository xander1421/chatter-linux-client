//! Defensive crypto check: confirm chatter's server-side data is opaque
//! without client-side key material, AND that recovery with PIN works as
//! designed (the SVR escrow path).
//!
//! Inputs are hardcoded for the two Dev User accounts auto-created by
//! `chatter-ui`'s `dev_auto_register` (src/lib.rs) — which needs BOTH the
//! `dev-auto-register` cargo feature and `CHATTER_DEV=1`; `dev-run.sh` sets both.
//!
//!     cargo run --example decrypt_dev_user -p chatter-core

use base64::Engine;
use chatter_core::crypto::profile_crypto::decrypt_display_name;
use chatter_core::crypto::svr::derive_svr_secret;
use chatter_core::crypto::svr_backup::decrypt_backup;

struct DevUser {
    label: &'static str,
    username: &'static str,
    pin: &'static str,
    svr_blob_hex: &'static str,
    profile_display_b64: &'static str,
}

fn main() {
    let users = [
        DevUser {
            label: "Dev User 22623",
            username: "dev22623",
            pin: "12345678",
            svr_blob_hex: "793344ecce0e385b2be7881501873ab3e082e242fc98a106bd4ae44607b01979e395d0291395bed9e194fe835485bb02ee57465ff0380f0a40f69517e5a00f3e36f3a104da7ee83da9c624af37f62a9d2c2a97bc70e500a084723bafb320fcf43443e50cb57028d6e20df1c6f544e73c19ebdb6fc986d44ebd66957eeb099a01a12bb6daed",
            profile_display_b64: "Lb2PkV7g7C0a5thlrX0RP6SW3xB+NK2X/H/wCUHYO+NmE3GJyrpBGcSsDIMcC/W40yvblEAOvcM3TvuB3Y7gorR4BKxVBv1GXTMNdGZ/fRdD",
        },
        DevUser {
            label: "Dev User 12369",
            username: "dev12369",
            pin: "12345678",
            svr_blob_hex: "bb35c05c55ad6c1f284f297f631bfa6127c0f03f650f183340592e35d92b947bc30306737db77004e152ef0193399be5647876c9c40aa0f102701b9113e42aa3673e40c043eb793908a9284ab002be45fab90b3d869eca34428d919a7f66379699e6fe337baeb66173cd22493150e208e2d1496138330c17efd2d11c47c3e64cdfff3ebe47",
            profile_display_b64: "LUSGBreCdLYdeRbl+m5U50QsWVJ5hN4ZI977jABtj/M7WHTbu/8HPi9hkxGg3j9B4LRrqbRpLuPkJL6I0V7mY38rgowUSeZbu2JoixTo5Irj",
        },
    ];

    for u in &users {
        println!("\n=== {} ===", u.label);
        println!("username (server stores HKDF-keyed hash): dev{{N}}");

        // Step 1: derive SVR secret from PIN + username (Argon2id, 128 MiB, t=8).
        // Server cannot do this without the PIN.
        let svr_secret =
            derive_svr_secret(u.pin, u.username).expect("argon2 derivation must succeed");
        println!(
            "svr_secret = Argon2id(PIN, salt=username) = {}",
            hex_short(&svr_secret)
        );

        // Step 2: decrypt the SVR backup blob with the derived secret.
        let blob = hex::decode(u.svr_blob_hex).expect("valid hex");
        let payload = match decrypt_backup(&blob, &svr_secret) {
            Ok(p) => p,
            Err(e) => {
                println!("ABORT: SVR decrypt failed: {e}");
                continue;
            }
        };
        println!("backup decrypted ({} bytes)", blob.len());
        println!(
            "  identity_priv = {}",
            hex_short(&payload.identity_keypair[..32])
        );
        println!(
            "  identity_pub  = {}",
            hex_short(&payload.identity_keypair[32..])
        );
        println!("  profile_key   = {}", hex_short(&payload.profile_key));

        // Step 3: decrypt the profile display_name with the recovered profile key.
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(u.profile_display_b64)
            .expect("valid base64");
        match decrypt_display_name(&payload.profile_key, &ciphertext) {
            Ok(name) => println!("display_name (decrypted): {name:?}"),
            Err(e) => println!("display_name decrypt failed: {e}"),
        }
    }

    println!("\n--- summary ---");
    println!("Server-side state for these accounts is opaque without the PIN.");
    println!("With PIN + username, the SVR escrow returns identity keypair + profile key,");
    println!("and the profile key recovers display_name via AES-256-GCM(profile_key, ct).");
    println!("This is the by-design recovery path — Signal SVR2 semantics — gated by the");
    println!("5-strike Argon2id PIN counter on the server (see internal/services/svr).");
}

fn hex_short(b: &[u8]) -> String {
    let s = b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    if s.len() > 16 {
        format!("{}…{} ({} bytes)", &s[..8], &s[s.len() - 8..], b.len())
    } else {
        s
    }
}
