//! Benchmark: Mutex<Ring> vs tokio::mpsc for log buffering.
//!
//! Tests the hot path: N producer threads each push 10_000 log lines,
//! one consumer thread drains them. Measures total wall-clock time.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;

const CAPACITY: usize = 1000;
const PRODUCERS: usize = 4;
const MSGS_PER_PRODUCER: usize = 10_000;

// ─── Mutex<Ring> baseline ───────────────────────────────────────────────

#[derive(Clone)]
struct LogLine {
    ts_ms: u64,
    level: u8,
    message: String,
}

struct Ring {
    slots: Box<[Option<LogLine>]>,
    write_idx: usize,
    len: usize,
}

impl Ring {
    fn new() -> Self {
        let slots: Vec<Option<LogLine>> = (0..CAPACITY).map(|_| None).collect();
        Self {
            slots: slots.into_boxed_slice(),
            write_idx: 0,
            len: 0,
        }
    }

    fn push(&mut self, line: LogLine) {
        self.slots[self.write_idx] = Some(line);
        self.write_idx = (self.write_idx + 1) % CAPACITY;
        if self.len < CAPACITY {
            self.len += 1;
        }
    }

    fn oldest_idx(&self) -> usize {
        if self.len < CAPACITY {
            0
        } else {
            self.write_idx
        }
    }

    fn drain_all(&self) -> Vec<LogLine> {
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            let idx = (self.oldest_idx() + i) % CAPACITY;
            if let Some(line) = &self.slots[idx] {
                out.push(line.clone());
            }
        }
        out
    }
}

fn bench_mutex_ring(c: &mut Criterion) {
    c.bench_function("mutex_ring_push_drain", |b| {
        b.iter(|| {
            let ring = Arc::new(Mutex::new(Ring::new()));

            let handles: Vec<_> = (0..PRODUCERS)
                .map(|t| {
                    let ring = ring.clone();
                    thread::spawn(move || {
                        for i in 0..MSGS_PER_PRODUCER {
                            let line = LogLine {
                                ts_ms: 12345,
                                level: 1,
                                message: format!("thread {} msg {}", t, i),
                            };
                            ring.lock().unwrap().push(line);
                        }
                    })
                })
                .collect();

            for h in handles {
                h.join().unwrap();
            }

            // Consumer: drain all
            let ring = ring.lock().unwrap();
            let out = ring.drain_all();
            black_box(out.len());
        });
    });
}

// ─── tokio::mpsc baseline ───────────────────────────────────────────────

fn bench_tokio_mpsc(c: &mut Criterion) {
    c.bench_function("tokio_mpsc_push_drain", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<LogLine>(CAPACITY);

            let handles: Vec<_> = (0..PRODUCERS)
                .map(|t| {
                    let tx = tx.clone();
                    thread::spawn(move || {
                        for i in 0..MSGS_PER_PRODUCER {
                            let line = LogLine {
                                ts_ms: 12345,
                                level: 1,
                                message: format!("thread {} msg {}", t, i),
                            };
                            // try_send: non-blocking, drops on Full (same as ring overwrite)
                            let _ = tx.try_send(line);
                        }
                    })
                })
                .collect();

            // Drop the original sender so the channel closes when producers finish.
            drop(tx);

            for h in handles {
                h.join().unwrap();
            }

            // Consumer: drain all
            let mut out = Vec::new();
            while let Ok(line) = rx.try_recv() {
                out.push(line);
            }
            black_box(out.len());
        });
    });
}

// ─── Single-thread push throughput ──────────────────────────────────────

fn bench_single_thread_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_push_10k");

    group.bench_function("mutex_ring", |b| {
        let ring = Arc::new(Mutex::new(Ring::new()));
        b.iter(|| {
            for i in 0..10_000 {
                let line = LogLine {
                    ts_ms: 12345,
                    level: 1,
                    message: format!("msg {}", i),
                };
                ring.lock().unwrap().push(line);
            }
        });
    });

    group.bench_function("tokio_mpsc", |b| {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<LogLine>(CAPACITY);
        b.iter(|| {
            for i in 0..10_000 {
                let line = LogLine {
                    ts_ms: 12345,
                    level: 1,
                    message: format!("msg {}", i),
                };
                let _ = tx.try_send(line);
            }
            // Drain to prevent channel filling up
            while let Ok(_) = rx.try_recv() {}
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_mutex_ring,
    bench_tokio_mpsc,
    bench_single_thread_push
);
criterion_main!(benches);
