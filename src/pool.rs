use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time;

use crate::error::PoolError;

// ── Pool ──────────────────────────────────────────────────────────────────────

pub struct Pool {
    // std::sync::Mutex (not tokio's) because:
    //   1. We lock inside Drop, which is synchronous.
    //   2. The critical section is tiny — just a VecDeque push/pop, never an .await.
    //   Holding a std mutex across an .await would be unsound; we never do that.
    idle: Mutex<VecDeque<TcpStream>>,

    // Semaphore capacity == max_size.
    // Invariant: available_permits == max_size - checked_out
    // Idle connections do NOT hold permits; only checked-out PoolGuards do.
    // This means acquire() blocks only when every slot is actively in use,
    // and health-check-dropping an idle connection costs nothing permit-wise.
    semaphore: Arc<Semaphore>,

    target: SocketAddr,
    pub max_size: usize,
}

impl Pool {
    pub fn new(target: SocketAddr, max_size: usize) -> Arc<Self> {
        Arc::new(Pool {
            idle: Mutex::new(VecDeque::with_capacity(max_size)),
            semaphore: Arc::new(Semaphore::new(max_size)),
            target,
            max_size,
        })
    }

    // ── acquire ───────────────────────────────────────────────────────────────

    /// Check out a connection.  Blocks if all `max_size` slots are in use.
    ///
    /// Stale idle connections are silently discarded and replaced — the
    /// permit stays valid throughout so no slot is lost.
    pub async fn acquire(self: &Arc<Self>) -> Result<PoolGuard, PoolError> {
        // Taking a permit is the chokepoint that enforces max_size.
        // acquire_owned() needs Arc<Semaphore> (not &Semaphore) so that
        // OwnedSemaphorePermit can outlive this stack frame.
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| PoolError::Closed)?;

        // Walk the idle queue, skipping connections the server has since closed.
        loop {
            // Lock, pop, immediately drop the guard — never hold across .await.
            let candidate = self.idle.lock().unwrap().pop_front();

            match candidate {
                Some(conn) if is_alive(&conn) => {
                    return Ok(PoolGuard {
                        conn: Some(conn),
                        pool: Arc::clone(self),
                        _permit: permit,
                    });
                }
                Some(_stale) => {
                    // Drop the stale TcpStream, try the next idle one.
                }
                None => {
                    // Idle queue is empty; we hold a permit so we're within max_size.
                    let conn = TcpStream::connect(self.target)
                        .await
                        .map_err(PoolError::Connect)?;
                    return Ok(PoolGuard {
                        conn: Some(conn),
                        pool: Arc::clone(self),
                        _permit: permit,
                    });
                }
            }
        }
    }

    /// Like `acquire`, but returns `PoolError::Timeout` if no slot is free within `dur`.
    pub async fn acquire_timeout(
        self: &Arc<Self>,
        dur: Duration,
    ) -> Result<PoolGuard, PoolError> {
        time::timeout(dur, self.acquire())
            .await
            .map_err(|_| PoolError::Timeout)?
    }

    // ── health check ──────────────────────────────────────────────────────────

    /// Sweep idle connections and drop any that have gone stale.
    ///
    /// Safe to call from a background task: e.g.
    ///   `tokio::spawn(async move { loop { pool.health_check(); sleep(30s).await; } })`
    pub fn health_check(&self) {
        self.idle.lock().unwrap().retain(|conn| is_alive(conn));
    }

    // ── stats ─────────────────────────────────────────────────────────────────

    pub fn stats(&self) -> Stats {
        let idle = self.idle.lock().unwrap().len();
        // available_permits = max_size - checked_out  →  checked_out = max_size - available
        let checked_out = self.max_size.saturating_sub(self.semaphore.available_permits());
        Stats { idle, checked_out }
    }
}

// ── PoolGuard ─────────────────────────────────────────────────────────────────

/// RAII handle for a checked-out connection.
///
/// Returning it to the pool is automatic: Drop pushes the connection back to
/// the idle queue *before* the OwnedSemaphorePermit is dropped, so the next
/// waiter is only woken after the connection is already available.
pub struct PoolGuard {
    conn: Option<TcpStream>,
    pool: Arc<Pool>,
    // Declared last so it drops after `conn` and `pool`.
    // Drop order == declaration order in Rust, so the semaphore is
    // incremented only after the connection is already in the idle queue.
    _permit: OwnedSemaphorePermit,
}

impl PoolGuard {
    pub fn conn(&mut self) -> &mut TcpStream {
        self.conn.as_mut().expect("connection already consumed")
    }

    /// Poison the connection so it is dropped rather than returned to the pool.
    /// Call this after a partial write, protocol error, or any state you can't recover.
    pub fn invalidate(&mut self) {
        self.conn = None;
    }
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // If the mutex is poisoned (a previous holder panicked), discard
            // the connection rather than propagating the poison.
            if let Ok(mut idle) = self.pool.idle.lock() {
                idle.push_back(conn);
            }
        }
        // _permit drops here: semaphore.available_permits() += 1 → wakes next waiter.
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Non-blocking liveness probe.
///
/// try_read() queries the socket's cached readiness state from the tokio reactor
/// without scheduling a new poll — safe to call outside an async context.
///
///   WouldBlock  → socket is open and idle (the common case)
///   Ok(0)       → server sent EOF; connection is dead
///   Err(_)      → broken pipe / reset / etc
fn is_alive(conn: &TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match conn.try_read(&mut buf) {
        Ok(0) => false,
        Ok(_) => true, // unexpected data in flight, but alive
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

// ── Stats ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Stats {
    pub idle: usize,
    pub checked_out: usize,
}

impl Stats {
    pub fn total(&self) -> usize {
        self.idle + self.checked_out
    }
}
