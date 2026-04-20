mod error;
mod pool;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use pool::Pool;

const CONCURRENT_TASKS: usize = 100;
const POOL_MAX_SIZE: usize = 5;
const ROUNDS: usize = 1_000;

// Injected at connection-accept time, not per-request.
// This models real-world one-time costs: TLS handshake, auth token exchange,
// DNS resolution, database login protocol, etc.
// With per-request latency both strategies would pay equally — the speedup
// only appears when connection *setup* is expensive and requests are cheap.
const CONNECT_LATENCY_MS: u64 = 2;

// ── echo server ───────────────────────────────────────────────────────────────

async fn start_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Simulates TLS handshake / auth / DNS — paid once per connection.
                tokio::time::sleep(Duration::from_millis(CONNECT_LATENCY_MS)).await;
                let (mut r, mut w) = socket.split();
                tokio::io::copy(&mut r, &mut w).await.ok();
            });
        }
    });
    addr
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn ping(conn: &mut TcpStream) {
    conn.write_all(b"ping\n").await.unwrap();
    let mut buf = [0u8; 5];
    conn.read_exact(&mut buf).await.unwrap();
}

/// Nearest-rank percentile on a pre-sorted slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx]
}

fn print_stats(label: &str, mut samples: Vec<f64>) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let total: f64 = samples.iter().sum();
    let mean = total / samples.len() as f64;
    println!(
        "  {label:<8}  total={:8.1} ms   mean={:6.3} ms   \
         p50={:6.3} ms   p95={:6.3} ms   p99={:6.3} ms",
        total,
        mean,
        percentile(&samples, 50.0),
        percentile(&samples, 95.0),
        percentile(&samples, 99.0),
    );
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let addr = start_echo_server().await;
    println!(
        "Echo server on {addr}  \
         ({}ms artificial connection-setup latency)\n",
        CONNECT_LATENCY_MS
    );

    let pool = Pool::new(addr, POOL_MAX_SIZE);

    // ─────────────────────────────────────────────────────────────────────────
    // 1.  Concurrent load: 100 tasks, pool capped at 5
    //
    //     95 tasks block in acquire() waiting for a semaphore permit.
    //     Tokio parks them — no busy-waiting.  The pool opens at most 5
    //     TCP connections regardless of how many tasks are spawned.
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Concurrent load: {CONCURRENT_TASKS} tasks, max pool size {POOL_MAX_SIZE} ===");

    let start = Instant::now();
    let mut handles = Vec::with_capacity(CONCURRENT_TASKS);

    for task_id in 0..CONCURRENT_TASKS {
        let p = Arc::clone(&pool);
        handles.push(tokio::spawn(async move {
            let mut guard = p.acquire().await.expect("acquire failed");
            ping(guard.conn()).await;
            task_id
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    println!(
        "{CONCURRENT_TASKS} tasks done in {:?}  \
         (pool opened at most {POOL_MAX_SIZE} TCP connections)",
        start.elapsed()
    );
    println!("Pool state: {:?}\n", pool.stats());

    // ─────────────────────────────────────────────────────────────────────────
    // 2.  Health check
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Health check ===");
    let before = pool.stats();
    pool.health_check();
    println!("  Before: {before:?}");
    println!("  After : {:?}\n", pool.stats());

    // ─────────────────────────────────────────────────────────────────────────
    // 3.  Invalidate demo
    // ─────────────────────────────────────────────────────────────────────────
    println!("=== Invalidate demo ===");
    {
        let mut guard = pool.acquire().await.unwrap();
        println!("  Simulating protocol error → invalidate()");
        guard.invalidate();
    }
    println!("  Pool state after invalidate: {:?}\n", pool.stats());

    // ─────────────────────────────────────────────────────────────────────────
    // 4.  Benchmark: pooled vs fresh  —  {ROUNDS} sequential round-trips
    //
    //     A fresh pool is created here so neither side benefits from the
    //     pre-warmed connections used in the concurrent demo above.
    //
    //     Pooled:  the first POOL_MAX_SIZE requests pay the 2ms setup cost;
    //              the remaining (ROUNDS - POOL_MAX_SIZE) reuse idle connections.
    //              Amortised setup cost → near zero per request.
    //
    //     Fresh:   every request opens a new connection, paying 2ms every time.
    //
    //     Expected speedup: ~(ROUNDS * 2ms) / (POOL_MAX_SIZE * 2ms) = ~200×
    // ─────────────────────────────────────────────────────────────────────────
    println!(
        "=== Benchmark: {ROUNDS} sequential round-trips \
         ({}ms connection latency, fresh pool) ===\n",
        CONNECT_LATENCY_MS
    );

    let bench_pool = Pool::new(addr, POOL_MAX_SIZE);

    // Warm-up: establish all POOL_MAX_SIZE connections before timing starts.
    {
        let mut warmup = Vec::with_capacity(POOL_MAX_SIZE);
        for _ in 0..POOL_MAX_SIZE {
            warmup.push(bench_pool.acquire().await.unwrap());
        }
        for mut g in warmup {
            ping(g.conn()).await;
        } // all guards drop here → connections return to idle
    }

    // Pooled
    let mut pooled = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let mut g = bench_pool.acquire().await.unwrap();
        ping(g.conn()).await;
        pooled.push(t.elapsed().as_secs_f64() * 1_000.0);
    }

    // Fresh connection every request
    let mut fresh = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let mut conn = TcpStream::connect(addr).await.unwrap();
        ping(&mut conn).await;
        fresh.push(t.elapsed().as_secs_f64() * 1_000.0);
    }

    let pooled_total: f64 = pooled.iter().sum();
    let fresh_total: f64 = fresh.iter().sum();

    print_stats("Pooled", pooled);
    print_stats("Fresh", fresh);
    println!(
        "\n  Speedup: {:.0}×  ({:.0} ms vs {:.0} ms total)",
        fresh_total / pooled_total,
        pooled_total,
        fresh_total,
    );

    println!("\nFinal pool state: {:?}", bench_pool.stats());
}
