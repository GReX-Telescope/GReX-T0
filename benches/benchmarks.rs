use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grex_t0::{
    common::{payload_start_time, Payload},
    dumps::DumpRing,
};
use hifitime::Epoch;

pub fn push_ring(c: &mut Criterion) {
    let mut dr = DumpRing::new(15);
    let pl = Payload::default();
    c.bench_function("push ring", |b| {
        b.iter(|| {
            dr.push(black_box(&pl));
        })
    });
}

pub fn inject_complex(c: &mut Criterion) {
    let mut payload = Payload::default();
    let pulse_time_slice = [0.0f64; 2048];
    c.bench_function("complex injection", |b| {
        b.iter(|| {
            for (payload_val, pulse_val) in payload.pol_a.iter_mut().zip(pulse_time_slice) {
                payload_val.0.re += (pulse_val).round() as i8;
            }
            for (payload_val, pulse_val) in payload.pol_b.iter_mut().zip(pulse_time_slice) {
                payload_val.0.re += (pulse_val).round() as i8;
            }
        })
    });
}

pub fn dump_ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("dump_ring");
    let _2_n = 5;
    let mut dr = DumpRing::new(_2_n); // 2^20 samples ~ 8GB
    let pl = Payload::default();
    // Fill the dump ring
    for _ in 0..2u32.pow(_2_n) {
        dr.push(&pl);
    }
    // Make sure a time exists for dump to correctly offset the samples
    let mut ps = payload_start_time().lock().unwrap();
    *ps = Some(Epoch::now().unwrap());
    // Only run this 10 times
    group.sample_size(10);
    // Benchmark creating the CDF file and writing to disk
    group.bench_function("dump_ring", |b| {
        b.iter(|| dr.dump(&std::env::temp_dir(), "test.nc"))
    });
    group.finish();
}

criterion_group!(benches, push_ring, inject_complex, dump_ring);
criterion_main!(benches);
