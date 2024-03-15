use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grex_t0::{common::Payload, dumps::DumpRing};

pub fn push_ring(c: &mut Criterion) {
    let mut dr = DumpRing::new(15);
    let pl = Payload::default();
    c.bench_function("push ring", |b| {
        b.iter(|| {
            dr.next_push().clone_from(black_box(&pl));
        })
    });
}

pub fn to_ndarray(c: &mut Criterion) {
    let payload = Payload::default();
    c.bench_function("payload to nd", |b| {
        b.iter(|| black_box(payload.into_ndarray()))
    });
}

pub fn inject_complex(c: &mut Criterion) {
    let mut payload = Payload::default();
    let pulse_time_slice = [0.0f64; 2048];
    c.bench_function("complex injection", |b| {
        b.iter(|| {
            for (payload_val, pulse_val) in payload.pol_a.iter_mut().zip(pulse_time_slice) {
                //let phase = (payload_val.0.im as f64).atan2(payload_val.0.re as f64);
                let phase = std::f64::consts::PI;
                payload_val.0.re += (pulse_val * phase).round() as i8;
                payload_val.0.im += (pulse_val * phase).round() as i8;
            }
            for (payload_val, pulse_val) in payload.pol_b.iter_mut().zip(pulse_time_slice) {
                //let phase = (payload_val.0.im as f64).atan2(payload_val.0.re as f64);
                let phase = std::f64::consts::PI;
                payload_val.0.re += (pulse_val * phase).round() as i8;
                payload_val.0.im += (pulse_val * phase).round() as i8;
            }
        })
    });
}

criterion_group!(benches, push_ring, to_ndarray, inject_complex);
criterion_main!(benches);
