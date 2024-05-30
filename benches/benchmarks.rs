use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grex_t0::{common::Payload, dumps::DumpRing, injection::inject};

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
    let slice = [123i8; 2048];
    c.bench_function("injection", |b| b.iter(|| inject(&mut payload, &slice)));
}

criterion_group!(benches, push_ring, injection);
criterion_main!(benches);
