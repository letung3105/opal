mod common;

use common::{get_bitcask, get_dashmap, get_sled, prebuilt_kv_pairs};
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Bencher, Criterion, Throughput};
use opal::engine::{self, KeyValueStore};
use pprof::criterion::{Output, PProfProfiler};
use rand::prelude::*;
use tempfile::TempDir;

const ITER: usize = 10000;
const KEY_SIZE: usize = 1000;
const VAL_SIZE: usize = 10000;

/// Call set on the same key-value store instance for every benchmark iteration, the key and
/// value will be randomly generated bytes sequences with size of `KEY_SIZE` and `VAL_SIZE`
pub fn bench_write(c: &mut Criterion) {
    let kv_pairs = prebuilt_kv_pairs(ITER, KEY_SIZE, VAL_SIZE);
    let mut nbytes = 0;
    for (k, v) in kv_pairs.iter() {
        nbytes += k.len() + v.len();
    }

    let mut g = c.benchmark_group("compare_engines_sequential_write");
    g.throughput(Throughput::Bytes(nbytes as u64));

    g.bench_with_input(
        "bitcask",
        &(&kv_pairs, engine::Type::BitCask),
        sequential_write_bulk_bench,
    );
    g.bench_with_input(
        "sled",
        &(&kv_pairs, engine::Type::Sled),
        sequential_write_bulk_bench,
    );
    g.bench_with_input(
        "dashmap",
        &(&kv_pairs, engine::Type::DashMap),
        sequential_write_bulk_bench,
    );
    g.finish();
}

fn sequential_write_bulk_bench(
    b: &mut Bencher,
    (kv_pairs, engine): &(&Vec<(Vec<u8>, Vec<u8>)>, engine::Type),
) {
    match *engine {
        engine::Type::BitCask => {
            b.iter_batched(
                || {
                    let (engine, tmpdir) = get_bitcask();
                    (engine, kv_pairs.to_vec(), tmpdir)
                },
                sequential_write_bulk_bench_iter,
                BatchSize::SmallInput,
            );
        }
        engine::Type::Sled => {
            b.iter_batched(
                || {
                    let (engine, tmpdir) = get_sled();
                    (engine, kv_pairs.to_vec(), tmpdir)
                },
                sequential_write_bulk_bench_iter,
                BatchSize::SmallInput,
            );
        }
        engine::Type::DashMap => {
            b.iter_batched(
                || {
                    let (engine, tmpdir) = get_dashmap();
                    (engine, kv_pairs.to_vec(), tmpdir)
                },
                sequential_write_bulk_bench_iter,
                BatchSize::SmallInput,
            );
        }
    }
}

fn sequential_write_bulk_bench_iter<E>(
    (engine, kv_pairs, _tmpdir): (E, Vec<(Vec<u8>, Vec<u8>)>, TempDir),
) where
    E: KeyValueStore,
{
    kv_pairs.into_iter().for_each(|(k, v)| {
        engine.set(black_box(&k), black_box(&v)).unwrap();
    });
}

/// Call get on a pre-populted key-value store instance for every benchmark iteration, the key
/// and value will be randomly generated bytes sequences with size of `KEY_SIZE` and `VAL_SIZE`
pub fn bench_read(c: &mut Criterion) {
    let kv_pairs = prebuilt_kv_pairs(ITER, KEY_SIZE, VAL_SIZE);
    let mut nbytes = 0;
    for (k, v) in kv_pairs.iter() {
        nbytes += k.len() + v.len();
    }

    let mut g = c.benchmark_group("compare_engines_sequential_read");
    g.throughput(Throughput::Bytes(nbytes as u64));

    {
        let (engine, _tmpdir) = get_bitcask();
        g.bench_with_input("bitcask", &(engine, &kv_pairs), sequential_read_bulk_bench);
    }
    {
        let (engine, _tmpdir) = get_sled();
        g.bench_with_input("sled", &(engine, &kv_pairs), sequential_read_bulk_bench);
    }
    {
        let (engine, _tmpdir) = get_dashmap();
        g.bench_with_input("dashmap", &(engine, &kv_pairs), sequential_read_bulk_bench);
    }
    g.finish();
}

fn sequential_read_bulk_bench<E>(
    b: &mut Bencher,
    (engine, kv_pairs): &(E, &Vec<(Vec<u8>, Vec<u8>)>),
) where
    E: KeyValueStore,
{
    kv_pairs.iter().cloned().for_each(|(k, v)| {
        engine.set(&k, &v).unwrap();
    });

    b.iter_batched(
        || {
            let mut kv_pairs = kv_pairs.to_vec();
            kv_pairs.shuffle(&mut rand::thread_rng());
            kv_pairs
        },
        |kv_pairs| {
            kv_pairs.into_iter().for_each(|(k, v)| {
                engine.get(black_box(&k)).unwrap();
            });
        },
        BatchSize::SmallInput,
    );
}

criterion_group!(
    name = benches;
    config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = bench_write, bench_read
);
criterion_main!(benches);