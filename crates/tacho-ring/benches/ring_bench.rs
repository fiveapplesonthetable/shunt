#![cfg(not(loom))]

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use tacho_ring::{Ring, CHUNK_SIZE};

fn bench_write_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_throughput");

    for &size in &[64usize, 256, 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let mut ring = Ring::new(8 * 1024 * 1024); // 8 MiB ring
            let packet = vec![0x42u8; sz];
            b.iter(|| {
                ring.write(black_box(&packet)).unwrap();
            });
        });
    }

    group.finish();
}

fn bench_snapshot_full_ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot_full_ring");
    let ring_size = 64 * 1024 * 1024; // 64 MiB
    group.throughput(Throughput::Bytes(ring_size as u64));

    // Prepare a full 64 MiB ring
    let mut ring = Ring::new(ring_size);
    let packet = vec![0xAAu8; 1024];
    let writes_to_fill = (ring_size / CHUNK_SIZE) * 4;
    for _ in 0..writes_to_fill {
        ring.write(&packet).unwrap();
    }

    group.bench_function("64_mib_snapshot", |b| {
        b.iter(|| {
            let mut total_bytes = 0usize;
            let stats = ring.snapshot(&mut |chunk| {
                total_bytes += chunk.len();
            });
            black_box((stats, total_bytes));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_write_throughput, bench_snapshot_full_ring);
criterion_main!(benches);
