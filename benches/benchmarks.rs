use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grex_t0::{
    common::{stokes_i, Payload, CHANNELS},
    dumps::DumpRing,
    injection::inject,
};

pub fn push_ring(c: &mut Criterion) {
    let mut dr = DumpRing::new(15);
    let pl = Payload::default();
    c.bench_function("push ring", |b| {
        b.iter(|| {
            dr.push(black_box(&pl));
        })
    });
}

pub fn injection(c: &mut Criterion) {
    let mut payload = Payload::default();
    let slice = [123i8; CHANNELS];
    c.bench_function("injection", |b| b.iter(|| inject(&mut payload, &slice)));
}

pub fn stokes(c: &mut Criterion) {
    let payload = Payload::default();
    let mut buf = [0f32; CHANNELS];
    c.bench_function("stokes_i", |b| b.iter(|| stokes_i(&mut buf, &payload)));
}

criterion_group!(benches, push_ring, injection, stokes);
criterion_main!(benches);
