//! V059: Mesh favourites, durably.
//!
//! Favourites are the courier trust tier: a mutual favourite may deposit five
//! envelopes with us, any signature-verified stranger only two, and the verified
//! pool can never crowd out favourites. Holding that set in memory meant every
//! restart silently demoted every favourite to the stranger quota — no error, no
//! visible change, just less of the user's mail getting carried.
//!
//! Keyed by Noise static key, which is the identity the courier tiers are
//! actually decided on. A peer ID would not do: it rotates.

use super::Migration;

pub struct V059MeshFavorites;

impl Migration for V059MeshFavorites {
    fn version(&self) -> u32 {
        59
    }

    fn description(&self) -> &'static str {
        "Durable bitchat mesh favourites (courier trust tier)"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE IF NOT EXISTS mesh_favorites (
            noise_key  BLOB PRIMARY KEY NOT NULL CHECK (length(noise_key) = 32),
            added_at   TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS mesh_favorites;")
    }
}
