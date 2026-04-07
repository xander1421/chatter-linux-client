//! Migration v049: Vector embeddings for local semantic search.
//!
//! Creates table for storing message embeddings locally. Enables
//! semantic search over E2E encrypted channels where the server
//! cannot search plaintext.

use super::Migration;

pub struct V049VectorEmbeddings;

impl Migration for V049VectorEmbeddings {
    fn version(&self) -> u32 {
        49
    }

    fn description(&self) -> &'static str {
        "Create vector embeddings table for local semantic search"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE vector_embeddings (
            id TEXT PRIMARY KEY,
            -- Source reference: what this embedding represents
            source_type INTEGER NOT NULL,  -- 0=message, 1=memory, 2=summary
            source_id TEXT NOT NULL,        -- message_id, memory_id, etc.
            channel_id TEXT,               -- optional: for scoped search
            -- The text that was embedded (needed for display in results)
            content_preview TEXT NOT NULL,
            -- Embedding vector stored as little-endian f32 BLOB
            embedding BLOB NOT NULL,
            -- Metadata
            model TEXT NOT NULL,           -- embedding model used (e.g. "nomic-embed-text")
            dimension INTEGER NOT NULL,    -- vector dimension for validation
            created_at TEXT NOT NULL
        );

        -- Index for channel-scoped search (most common: search within a channel)
        CREATE INDEX idx_vector_embeddings_channel
            ON vector_embeddings(channel_id, source_type);

        -- Index for source lookups (check if message already indexed)
        CREATE UNIQUE INDEX idx_vector_embeddings_source
            ON vector_embeddings(source_type, source_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS vector_embeddings;")
    }
}
