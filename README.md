# tcp-pool — Async TCP Connection Pooler in Rust

A production-grade connection pooler built from scratch in async Rust, 
demonstrating 99× latency improvement over fresh connections under concurrent load.

## Results

| Strategy | Total (1000 reqs) | Mean | p50 | p95 | p99 |
|----------|------------------|------|-----|-----|-----|
| Pooled   | 35.9 ms | 0.036 ms | 0.031 ms | 0.058 ms | 0.075 ms |
| Fresh    | 3548.9 ms | 3.549 ms | 3.613 ms | 3.653 ms | 3.678 ms |

**Speedup: 99× (36 ms vs 3549 ms total)**

## How it works

- Maintains a pool of N pre-opened TCP connections
- `acquire()` — checks out an available connection instantly
- `release()` — returns connection to pool for reuse
- Health checking — detects and replaces stale connections
- `invalidate()` — drops poisoned connections after protocol errors
- Timeout-aware acquire with custom `PoolError` types

## Architecture

- Async Rust with Tokio
- `Arc<Mutex<>>` for thread-safe pool state
- Semaphore-based backpressure — 100 concurrent tasks, max 5 connections
- p50/p95/p99 latency percentiles

## Run it

```bash
cargo run --release
```
