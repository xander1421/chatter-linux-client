//! Vector embedding repository.
//!
//! Stores and retrieves vector embeddings for local semantic search.
//! Embeddings are stored as little-endian f32 BLOBs in SQLite.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::params;

use crate::error::StorageError;
use crate::storage::Database;

/// Source type for what was embedded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum EmbeddingSourceType {
    Message = 0,
    Memory = 1,
    Summary = 2,
}

impl EmbeddingSourceType {
    fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Message,
            1 => Self::Memory,
            2 => Self::Summary,
            _ => Self::Message,
        }
    }
}

/// A stored vector embedding with metadata.
#[derive(Debug, Clone)]
pub struct StoredEmbedding {
    pub id: String,
    pub source_type: EmbeddingSourceType,
    pub source_id: String,
    pub channel_id: Option<String>,
    pub content_preview: String,
    pub embedding: Vec<f32>,
    pub model: String,
    pub dimension: usize,
}

/// Repository for vector embedding storage.
pub struct VectorRepository {
    db: Arc<Database>,
}

impl VectorRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Store an embedding. Replaces existing embedding for the same source.
    pub fn upsert(&self, embedding: &StoredEmbedding) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let blob = f32_vec_to_blob(&embedding.embedding);
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO vector_embeddings (id, source_type, source_id, channel_id, content_preview, embedding, model, dimension, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(source_type, source_id) DO UPDATE SET
                embedding = excluded.embedding,
                content_preview = excluded.content_preview,
                model = excluded.model,
                dimension = excluded.dimension,
                created_at = excluded.created_at",
            params![
                embedding.id,
                embedding.source_type as i32,
                embedding.source_id,
                embedding.channel_id,
                embedding.content_preview,
                blob,
                embedding.model,
                embedding.dimension as i32,
                now,
            ],
        )?;
        Ok(())
    }

    /// Store multiple embeddings in a single transaction.
    pub fn upsert_batch(&self, embeddings: &[StoredEmbedding]) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        let mut count = 0u32;

        for embedding in embeddings {
            let blob = f32_vec_to_blob(&embedding.embedding);
            tx.execute(
                "INSERT INTO vector_embeddings (id, source_type, source_id, channel_id, content_preview, embedding, model, dimension, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(source_type, source_id) DO UPDATE SET
                    embedding = excluded.embedding,
                    content_preview = excluded.content_preview,
                    model = excluded.model,
                    dimension = excluded.dimension,
                    created_at = excluded.created_at",
                params![
                    embedding.id,
                    embedding.source_type as i32,
                    embedding.source_id,
                    embedding.channel_id,
                    embedding.content_preview,
                    blob,
                    embedding.model,
                    embedding.dimension as i32,
                    now,
                ],
            )?;
            count += 1;
        }

        tx.commit()?;
        Ok(count)
    }

    /// Check if a source has already been indexed.
    pub fn is_indexed(
        &self,
        source_type: EmbeddingSourceType,
        source_id: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vector_embeddings WHERE source_type = ?1 AND source_id = ?2",
            params![source_type as i32, source_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Load all embeddings for a channel (for in-memory search).
    pub fn load_channel_embeddings(
        &self,
        channel_id: &str,
        source_type: EmbeddingSourceType,
    ) -> Result<Vec<StoredEmbedding>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_type, source_id, channel_id, content_preview, embedding, model, dimension
             FROM vector_embeddings
             WHERE channel_id = ?1 AND source_type = ?2
             ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![channel_id, source_type as i32])?;
        let mut results = Vec::new();

        while let Some(row) = rows.next()? {
            results.push(row_to_embedding(row)?);
        }

        Ok(results)
    }

    /// Load all embeddings (for global search across channels).
    pub fn load_all_embeddings(
        &self,
        source_type: EmbeddingSourceType,
    ) -> Result<Vec<StoredEmbedding>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, source_type, source_id, channel_id, content_preview, embedding, model, dimension
             FROM vector_embeddings
             WHERE source_type = ?1
             ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![source_type as i32])?;
        let mut results = Vec::new();

        while let Some(row) = rows.next()? {
            results.push(row_to_embedding(row)?);
        }

        Ok(results)
    }

    /// Delete an embedding by source.
    pub fn delete_by_source(
        &self,
        source_type: EmbeddingSourceType,
        source_id: &str,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM vector_embeddings WHERE source_type = ?1 AND source_id = ?2",
            params![source_type as i32, source_id],
        )?;
        Ok(())
    }

    /// Delete all embeddings for a channel.
    pub fn delete_channel_embeddings(&self, channel_id: &str) -> Result<u32, StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM vector_embeddings WHERE channel_id = ?1",
            params![channel_id],
        )?;
        Ok(rows as u32)
    }

    /// Count embeddings by channel.
    pub fn count_by_channel(&self, channel_id: &str) -> Result<u32, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM vector_embeddings WHERE channel_id = ?1",
            params![channel_id],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Count total embeddings.
    pub fn count_total(&self) -> Result<u32, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vector_embeddings", [], |row| {
            row.get(0)
        })?;
        Ok(count as u32)
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn row_to_embedding(row: &rusqlite::Row) -> Result<StoredEmbedding, rusqlite::Error> {
    let id: String = row.get(0)?;
    let source_type: i32 = row.get(1)?;
    let source_id: String = row.get(2)?;
    let channel_id: Option<String> = row.get(3)?;
    let content_preview: String = row.get(4)?;
    let blob: Vec<u8> = row.get(5)?;
    let model: String = row.get(6)?;
    let dimension: i32 = row.get(7)?;

    Ok(StoredEmbedding {
        id,
        source_type: EmbeddingSourceType::from_i32(source_type),
        source_id,
        channel_id,
        content_preview,
        embedding: blob_to_f32_vec(&blob),
        model,
        dimension: dimension as usize,
    })
}

/// Convert f32 vector to little-endian byte BLOB for SQLite storage.
fn f32_vec_to_blob(vec: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vec.len() * 4);
    for &f in vec {
        blob.extend_from_slice(&f.to_le_bytes());
    }
    blob
}

/// Convert little-endian byte BLOB back to f32 vector.
fn blob_to_f32_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_f32_blob_roundtrip() {
        let original = vec![1.0f32, -0.5, 0.0, std::f32::consts::PI, f32::MIN, f32::MAX];
        let blob = f32_vec_to_blob(&original);
        let recovered = blob_to_f32_vec(&blob);
        assert_eq!(original, recovered);
    }

    #[test]
    fn test_f32_blob_empty() {
        let original: Vec<f32> = vec![];
        let blob = f32_vec_to_blob(&original);
        let recovered = blob_to_f32_vec(&blob);
        assert_eq!(original, recovered);
    }
}
