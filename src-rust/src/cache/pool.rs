//! A small pool of read-only connections.
//!
//! The thumbnail serve path reads a blob per grid cell, and a scrolling grid
//! issues them in bursts. Those reads must not queue behind the single writer,
//! which is busy with the index pass at open and with tier writes afterwards —
//! so they go through their own connections, opened `SQLITE_OPEN_READ_ONLY` so
//! the read-only-ness is the database's rule rather than a convention.
//!
//! Two to six connections, from `available_parallelism`. More than that buys
//! nothing: under WAL the readers do not block each other, and each one costs
//! its own page cache and `mmap` window.
//!
//! `devices.db` gets the same shape in miniature — one writer, two readers —
//! because authentication runs on *every* thumbnail request and must never
//! queue behind a pairing write.

use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::Mutex;
use tokio::sync::Semaphore;

/// How many read connections to open, given the machine and a cap.
fn pool_size(max: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, max)
}

pub struct ReadPool {
    /// One permit per connection, so `acquire` waits rather than opening more.
    permits: Semaphore,
    idle: Mutex<Vec<Connection>>,
}

impl ReadPool {
    /// Open `min(available_parallelism, max)` read-only connections, each with
    /// the read-side PRAGMAs applied.
    pub fn open(path: &Path, max: usize, cache_size_kb: i64) -> rusqlite::Result<Self> {
        let n = pool_size(max);
        let mut idle = Vec::with_capacity(n);
        for _ in 0..n {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            apply_read_pragmas(&conn, cache_size_kb)?;
            idle.push(conn);
        }
        Ok(Self {
            permits: Semaphore::new(n),
            idle: Mutex::new(idle),
        })
    }

    /// Take a connection, waiting if every one is in use.
    pub async fn get(&self) -> PooledConn<'_> {
        let permit = self
            .permits
            .acquire()
            .await
            .expect("read pool semaphore is never closed");
        let conn = self
            .idle
            .lock()
            .expect("read pool mutex poisoned")
            .pop()
            .expect("a permit guarantees an idle connection");
        PooledConn {
            pool: self,
            conn: Some(conn),
            _permit: permit,
        }
    }
}

/// A borrowed read connection, returned to the pool on drop — including when
/// the future holding it is cancelled, which a scrolling grid causes
/// constantly.
pub struct PooledConn<'a> {
    pool: &'a ReadPool,
    conn: Option<Connection>,
    _permit: tokio::sync::SemaphorePermit<'a>,
}

impl std::ops::Deref for PooledConn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection taken only in Drop")
    }
}

impl Drop for PooledConn<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool
                .idle
                .lock()
                .expect("read pool mutex poisoned")
                .push(conn);
        }
    }
}

/// The read side's PRAGMAs. `mmap_size` is per connection, which is why the
/// pool is small: six readers at 256 MB each is a lot of address space to hand
/// out for no gain.
pub fn apply_read_pragmas(conn: &Connection, cache_size_kb: i64) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA cache_size=-{cache_size_kb};
         PRAGMA temp_store=MEMORY;
         PRAGMA mmap_size=268435456;"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_is_clamped_at_both_ends() {
        assert!(pool_size(6) >= 2);
        assert!(pool_size(6) <= 6);
        assert_eq!(pool_size(2), 2);
    }

    #[tokio::test]
    async fn a_connection_returns_to_the_pool_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();

        let pool = ReadPool::open(&path, 2, 8_000).unwrap();
        for _ in 0..10 {
            let c = pool.get().await;
            let x: i64 = c.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
            assert_eq!(x, 1);
        }
        assert_eq!(pool.idle.lock().unwrap().len(), pool.permits.available_permits());
    }

    #[tokio::test]
    async fn the_pool_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (x INTEGER);")
            .unwrap();
        let pool = ReadPool::open(&path, 2, 8_000).unwrap();
        let c = pool.get().await;
        assert!(c.execute("INSERT INTO t VALUES (1)", []).is_err());
    }
}
