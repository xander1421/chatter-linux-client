//! User tables migration.
//!
//! Creates tables for users, profiles, devices, and friendships.

use super::Migration;

/// User tables migration (v002).
///
/// Creates the core user-related tables:
/// - `users`: Core identity with public key
/// - `profiles`: Display data (name, avatar, status)
/// - `devices`: Multi-device support with Signal registration IDs
/// - `friendships`: User relationships (pending, accepted, blocked)
pub struct V002Users;

impl Migration for V002Users {
    fn version(&self) -> u32 {
        2
    }

    fn description(&self) -> &'static str {
        "User tables - users, profiles, devices, friendships"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Users (core identity)
        CREATE TABLE users (
            id TEXT PRIMARY KEY,
            username TEXT NOT NULL UNIQUE,
            public_key BLOB NOT NULL,
            last_active TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX idx_users_username ON users(username);

        -- Profiles (display data, separate for caching)
        CREATE TABLE profiles (
            user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            display_name TEXT NOT NULL,
            avatar_url TEXT,
            status_text TEXT,
            status_emoji TEXT,
            status_expires_at TEXT,
            presence INTEGER NOT NULL DEFAULT 0
        );

        -- Devices (multi-device support)
        CREATE TABLE devices (
            id INTEGER NOT NULL,
            user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            registration_id INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            last_seen TEXT NOT NULL,
            PRIMARY KEY (user_id, id)
        );

        -- Friendships (relationships)
        CREATE TABLE friendships (
            user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            friend_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            status INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (user_id, friend_id)
        );
        CREATE INDEX idx_friendships_friend ON friendships(friend_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_friendships_friend;
            DROP TABLE IF EXISTS friendships;
            DROP TABLE IF EXISTS devices;
            DROP TABLE IF EXISTS profiles;
            DROP INDEX IF EXISTS idx_users_username;
            DROP TABLE IF EXISTS users;
            "#,
        )
    }
}
