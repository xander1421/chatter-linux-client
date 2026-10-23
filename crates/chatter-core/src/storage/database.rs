//! SQLCipher database connection.
//!
//! Encrypted database connection pool management using SQLCipher.
//!
//! Architecture:
//! - Single writer connection protected by Mutex (serializes writes)
//! - Pool of reader connections for concurrent reads (WAL mode enables this)
//! - Reader pool creates connections lazily and reuses them

use crate::error::StorageError;
use crate::storage::migrations::default_migrations;
use parking_lot::{Condvar, Mutex, MutexGuard};
use rusqlite::{Connection, OpenFlags};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use zeroize::Zeroizing;

/// Number of reader connections in the pool.
const READER_POOL_SIZE: usize = 4;

/// SQLCipher database connection pool.
pub struct Database {
    /// Writer connection with mutex for thread-safe access.
    writer: Arc<Mutex<Connection>>,
    /// Pool of reader connections.
    readers: Arc<ReaderPool>,
    /// Path to database file.
    path: PathBuf,
    /// One core consumer of committed message inserts, independent of UI events.
    message_stores:
        Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::domain::message::Message>>>,
}

/// Pool of read-only connections for concurrent database reads.
struct ReaderPool {
    /// Available reader connections + condvar for waiters.
    available: Mutex<VecDeque<Connection>>,
    /// Notified when a connection is returned to the pool.
    returned: Condvar,
    /// Path for creating new connections.
    path: PathBuf,
    /// Passphrase for new connections.
    /// Wrapped in Zeroizing to ensure memory is cleared on drop.
    passphrase: Mutex<Zeroizing<String>>,
    /// Current number of connections (available + in-use). Atomic for race-free creation.
    count: AtomicUsize,
}

impl ReaderPool {
    fn new(path: PathBuf, passphrase: Zeroizing<String>) -> Self {
        Self {
            available: Mutex::new(VecDeque::with_capacity(READER_POOL_SIZE)),
            returned: Condvar::new(),
            path,
            passphrase: Mutex::new(passphrase),
            count: AtomicUsize::new(0),
        }
    }

    /// Acquire a reader connection from the pool.
    fn acquire(&self) -> Result<PooledReader<'_>, StorageError> {
        // Fast path: grab an available connection.
        {
            let mut available = self.available.lock();
            if let Some(conn) = available.pop_front() {
                return Ok(PooledReader {
                    conn: Some(conn),
                    pool: self,
                });
            }
        }

        // Try to create a new connection if pool isn't full.
        // compare_exchange prevents the race where two threads both see count < MAX
        // and both create a connection, exceeding the pool size.
        let current = self.count.load(Ordering::Relaxed);
        if current < READER_POOL_SIZE {
            if self
                .count
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return match self.open_reader() {
                    Ok(conn) => Ok(PooledReader {
                        conn: Some(conn),
                        pool: self,
                    }),
                    Err(e) => {
                        self.count.fetch_sub(1, Ordering::Relaxed);
                        Err(e)
                    }
                };
            }
        }

        // Pool is full and all connections are in use — wait up to 10s for one to be returned.
        let mut available = self.available.lock();
        let deadline = Duration::from_secs(10);
        let start = std::time::Instant::now();
        loop {
            if let Some(conn) = available.pop_front() {
                return Ok(PooledReader {
                    conn: Some(conn),
                    pool: self,
                });
            }
            let elapsed = start.elapsed();
            if elapsed >= deadline {
                return Err(StorageError::ConnectionPool(
                    "Reader pool exhausted: all connections in use for >10s".into(),
                ));
            }
            let remaining = deadline - elapsed;
            self.returned.wait_for(&mut available, remaining);
        }
    }

    /// Return a connection to the pool.
    fn release(&self, conn: Connection) {
        self.available.lock().push_back(conn);
        self.returned.notify_one();
    }

    /// Open a new read-only SQLCipher connection.
    fn open_reader(&self) -> Result<Connection, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&self.path, flags)?;

        // Set SQLCipher encryption key using a reference to avoid heap copies
        let passphrase = self.passphrase.lock();
        conn.pragma_update(None, "key", &**passphrase)
            .map_err(|e| StorageError::Encryption(format!("Failed to set reader key: {}", e)))?;
        // Pin KDF params (see writer-side comment).
        conn.pragma_update(None, "cipher_compatibility", "4")
            .map_err(|e| {
                StorageError::Encryption(format!("Failed to pin cipher_compatibility: {}", e))
            })?;
        conn.pragma_update(None, "kdf_iter", "256000")
            .map_err(|e| StorageError::Encryption(format!("Failed to pin kdf_iter: {}", e)))?;
        conn.pragma_update(None, "cipher_page_size", "4096")
            .map_err(|e| {
                StorageError::Encryption(format!("Failed to pin cipher_page_size: {}", e))
            })?;

        conn.execute_batch("SELECT count(*) FROM sqlite_master;")
            .map_err(|e| StorageError::Encryption(format!("Invalid reader key: {}", e)))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        conn.pragma_update(None, "cache_size", "-65536")?;
        // mmap intentionally not set: SQLCipher ignores on encrypted DBs.

        Ok(conn)
    }
}

/// A read-only connection borrowed from the pool.
///
/// Automatically returns to the pool when dropped.
pub struct PooledReader<'a> {
    conn: Option<Connection>,
    pool: &'a ReaderPool,
}

impl std::ops::Deref for PooledReader<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("connection taken")
    }
}

impl Drop for PooledReader<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
    }
}

impl Database {
    /// Replace the consumer when a new authenticated core session starts.
    pub(crate) fn subscribe_message_stores(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<crate::domain::message::Message> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.message_stores.lock() = Some(tx);
        tracing::info!(database = ?self.path, database_instance = ?std::ptr::from_ref(self), "Committed message store consumer subscribed");
        rx
    }

    pub(crate) fn announce_message_store(&self, message: &crate::domain::message::Message) {
        match self.message_stores.lock().as_ref() {
            Some(tx) if tx.send(message.clone()).is_err() => {
                tracing::warn!(database = ?self.path, database_instance = ?std::ptr::from_ref(self), message_id = %message.id,
                    "Committed message store consumer stopped");
            }
            None => {
                tracing::warn!(database = ?self.path, database_instance = ?std::ptr::from_ref(self), message_id = %message.id,
                    "Committed message store has no consumer");
            }
            Some(_) => {}
        }
    }

    /// Open an encrypted SQLCipher database.
    ///
    /// Creates the database file if it doesn't exist. The passphrase is used
    /// to encrypt all data at rest using SQLCipher.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the database file
    /// * `passphrase` - Encryption passphrase for SQLCipher
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Encryption` if the passphrase is invalid or
    /// encryption setup fails. Returns `StorageError::Database` for other
    /// database errors.
    pub fn open(path: &Path, passphrase: &str) -> Result<Self, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(path, flags)?;

        // Set SQLCipher encryption key
        // The passphrase must be set before any other operations
        conn.pragma_update(None, "key", passphrase).map_err(|e| {
            StorageError::Encryption(format!("Failed to set encryption key: {}", e))
        })?;

        // SECURITY: pin SQLCipher KDF parameters so a future library bump
        // can't silently lower them. SQLCipher 4 defaults are PBKDF2-SHA512
        // with 256k iterations + 4096-byte page size; we restate them
        // explicitly so the values are part of our schema contract, not a
        // dependency-version contract.
        conn.pragma_update(None, "cipher_compatibility", "4")
            .map_err(|e| {
                StorageError::Encryption(format!("Failed to pin cipher_compatibility: {}", e))
            })?;
        conn.pragma_update(None, "kdf_iter", "256000")
            .map_err(|e| StorageError::Encryption(format!("Failed to pin kdf_iter: {}", e)))?;
        conn.pragma_update(None, "cipher_page_size", "4096")
            .map_err(|e| {
                StorageError::Encryption(format!("Failed to pin cipher_page_size: {}", e))
            })?;

        // Verify the key is correct by selecting from sqlite_master
        // This works for both new and existing databases
        conn.execute_batch("SELECT count(*) FROM sqlite_master;")
            .map_err(|e| StorageError::Encryption(format!("Invalid encryption key: {}", e)))?;

        // Enable foreign key constraints
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Enable WAL mode for better concurrency (allows concurrent reads while writing)
        // This is essential for proactive DM session establishment which opens a second connection
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // busy_timeout: wait up to 5 seconds if database is locked by another connection
        // Without this, concurrent writes/reads that hit a lock fail immediately with SQLITE_BUSY.
        // 5s is generous for a desktop app — typical lock hold times are <10ms.
        conn.pragma_update(None, "busy_timeout", "5000")?;

        // synchronous=NORMAL is safe with WAL mode — data is still durable across crashes.
        // FULL (default) forces extra fsync on every commit which is unnecessary with WAL.
        // Signal Desktop uses the same setting.
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        // Performance optimizations for large BLOB reads (attachments)
        // cache_size: negative value = KB, so -65536 = 64MB cache (default is ~2MB)
        // This significantly speeds up reading large attachments from SQLCipher
        conn.pragma_update(None, "cache_size", "-65536")?;

        // Intentionally NOT setting `mmap_size`: SQLCipher disables
        // mmap-based I/O on encrypted databases (see
        // https://www.zetetic.net/sqlcipher/sqlcipher-api/#mmap_size). The
        // pragma is silently ignored today, but if a future SQLCipher build
        // ever loosened that, mapped pages would expose plaintext to the
        // OS page cache. Don't set it.

        // Create reader pool for concurrent read access
        let readers = Arc::new(ReaderPool::new(
            path.to_path_buf(),
            Zeroizing::new(passphrase.to_string()),
        ));

        Ok(Self {
            message_stores: Mutex::new(None),
            writer: Arc::new(Mutex::new(conn)),
            readers,
            path: path.to_path_buf(),
        })
    }

    /// Get a locked writer connection for database mutations.
    ///
    /// Use this for INSERT, UPDATE, DELETE operations.
    /// The returned guard holds the mutex lock until dropped.
    ///
    /// For read-only queries (SELECT), prefer `read_connection()` which
    /// allows concurrent access.
    pub fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        Ok(self.writer.lock())
    }

    /// Get a read-only connection for database queries.
    ///
    /// Use this for SELECT operations. Multiple readers can execute
    /// concurrently thanks to SQLite WAL mode.
    ///
    /// The connection is automatically returned to the pool when dropped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let conn = db.read_connection()?;
    /// let mut stmt = conn.prepare("SELECT * FROM messages WHERE channel_id = ?")?;
    /// let messages = stmt.query_map([channel_id], |row| { ... })?;
    /// ```
    pub fn read_connection(&self) -> Result<PooledReader<'_>, StorageError> {
        self.readers.acquire()
    }

    /// Get the path to the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a fresh read-only connection for streaming BLOB access.
    pub fn open_reader_connection(&self) -> Result<Connection, StorageError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(&self.path, flags)?;

        // Set SQLCipher encryption key using a reference from the reader pool
        let passphrase = self.readers.passphrase.lock();
        conn.pragma_update(None, "key", &**passphrase)
            .map_err(|e| StorageError::Encryption(format!("Failed to set reader key: {}", e)))?;
        // Pin KDF params (see writer-side comment).
        conn.pragma_update(None, "cipher_compatibility", "4")
            .map_err(|e| {
                StorageError::Encryption(format!("Failed to pin cipher_compatibility: {}", e))
            })?;
        conn.pragma_update(None, "kdf_iter", "256000")
            .map_err(|e| StorageError::Encryption(format!("Failed to pin kdf_iter: {}", e)))?;
        conn.pragma_update(None, "cipher_page_size", "4096")
            .map_err(|e| {
                StorageError::Encryption(format!("Failed to pin cipher_page_size: {}", e))
            })?;

        // Verify the key
        conn.execute_batch("SELECT count(*) FROM sqlite_master;")
            .map_err(|e| StorageError::Encryption(format!("Invalid reader key: {}", e)))?;

        // Set WAL mode for reads
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;

        // Performance optimizations for large BLOB streaming.
        // Intentionally NOT setting `mmap_size` - SQLCipher refuses mmap
        // on encrypted databases; see writer-side comment.
        conn.pragma_update(None, "cache_size", "-65536")?; // 64MB cache

        Ok(conn)
    }

    /// Explicitly close the database connection.
    ///
    /// This consumes the Database instance. Note that connections are also
    /// closed automatically when the Database is dropped.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::ConnectionPool` if the mutex is poisoned or
    /// if the connection cannot be unwrapped (other references exist).
    pub fn close(self) -> Result<(), StorageError> {
        // Try to get the inner writer connection
        let writer = Arc::try_unwrap(self.writer).map_err(|_| {
            StorageError::ConnectionPool(
                "Cannot close: other references to writer connection exist".into(),
            )
        })?;

        let conn = writer.into_inner();

        // Close the writer connection explicitly
        conn.close().map_err(|(_, e)| StorageError::Database(e))?;

        // Reader pool connections are closed when the ReaderPool is dropped

        Ok(())
    }

    /// Run all pending database migrations.
    ///
    /// Creates schema_version table if needed and applies any migrations
    /// that haven't been run yet. Safe to call multiple times.
    ///
    /// # Errors
    ///
    /// Returns `StorageError::Migration` if any migration fails.
    pub fn run_migrations(&self) -> Result<(), StorageError> {
        let conn = self.connection()?;
        let runner = default_migrations();
        runner.run(&conn)?;
        drop(conn);
        if let Err(error) = crate::storage::repositories::attachment_repo::AttachmentRepository::retry_pending_cleanup(self) {
            tracing::warn!(%error, "pending attachment cleanup will be retried");
        }
        Ok(())
    }
}
