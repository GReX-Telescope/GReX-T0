//! Common types shared between tasks

use arrayvec::ArrayVec;
use hifitime::prelude::*;
use ndarray::prelude::*;
use num_complex::Complex;
use pulp::{as_arrays, as_arrays_mut, cast, f32x8, i16x16, i32x8, x86::V3};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};

/// Number of frequency channels (set by gateware)
pub const CHANNELS: usize = 2048;
/// True packet cadence, set by the size of the FFT (4096) and the sampling time (2ns)
pub const PACKET_CADENCE: f64 = 8.192e-6;
/// Standard timeout for blocking ops
pub const BLOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Global atomic to hold the payload count of the first packet
pub static FIRST_PACKET: AtomicU64 = AtomicU64::new(0);

pub type Stokes = ArrayVec<f32, CHANNELS>;

/// Get the global, true packet start time of payload 0, not necessarily the first one we processed
pub fn payload_start_time() -> &'static Arc<Mutex<Option<Epoch>>> {
    static PACKET_START_TIME: OnceLock<Arc<Mutex<Option<Epoch>>>> = OnceLock::new();
    PACKET_START_TIME.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Get the true time of the data in a given payload count
pub fn payload_time(count: u64) -> Epoch {
    let payload_zero_time = payload_start_time().lock().unwrap().unwrap();
    payload_zero_time + Duration::from_seconds(count as f64 * PACKET_CADENCE)
}

/// Get the Epoch of the first payload we processed (not necessarily Payload 0)
pub fn processed_payload_start_time() -> Epoch {
    let first_processed_packet = FIRST_PACKET.load(Ordering::Acquire);
    payload_time(first_processed_packet)
}

/// The complex number representing the voltage of a single channel
#[derive(Debug, Clone, Copy)]
pub struct Channel(pub Complex<i8>);

impl Channel {
    pub fn new(re: i8, im: i8) -> Self {
        Self(Complex::new(re, im))
    }
}

pub type Channels = [Channel; CHANNELS];

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Payload {
    /// Number of packets since the first packet
    pub count: u64,
    pub pol_a: Channels,
    pub pol_b: Channels,
}

impl Default for Payload {
    fn default() -> Self {
        // Safety: Payload having a 0-bit pattern is valid
        unsafe { std::mem::zeroed() }
    }
}

impl Payload {
    /// Yields an [`ndarray::ArrayView3`] of dimensions (Polarization, Channel, Real/Imaginary)
    pub fn as_ndarray_data_view(&self) -> ArrayView3<i8> {
        // C-array format, so the pol_a, pol_b chunk is in memory as
        //        POL A               POL B
        //  CH1   CH2   CH3  ...  CH1   CH2   CH3
        // [R I] [R I] [R I] ... [R I] [R I] [R 1]
        // Which implies a tensor with dimensions Pols (2), Chan (2048), Reim (2)
        // As the first index is the slowest changing in row-major (C) languages
        let raw_ptr = self.pol_a.as_ptr();
        // Safety:
        // - The elements seen by moving ptr live as long 'self and are not mutably aliased
        // - The result of ptr.add() is non-null and aligned
        // - It is safe to .offset() the pointer repeatedely along all axes (it's all bytes)
        // - The stides are non-negative
        // - The product of the non-zero axis lenghts (2*CHANNELS*2) does not exceed isize::MAX
        unsafe {
            ArrayView::from_shape_ptr(
                (2, CHANNELS, 2),
                std::mem::transmute::<*const Channel, *const i8>(raw_ptr),
            )
        }
    }
}

fn simd_stokes(dst: &mut [f32; CHANNELS], a: &[i8; 2 * CHANNELS], b: &[i8; 2 * CHANNELS]) {
    if let Some(simd) = V3::try_new() {
        struct Impl<'a> {
            simd: V3,
            dst: &'a mut [f32],
            a: &'a [i8],
            b: &'a [i8],
        }

        impl pulp::NullaryFnOnce for Impl<'_> {
            type Output = ();

            #[inline(always)]
            fn call(self) -> Self::Output {
                let Self { simd, dst, a, b } = self;
                // Scale to normalize the floating point result
                let scale = cast([16384f32; 8]);
                // We want to exploint f32 FMA, which in AVX256 will work on f32x8 (once again no tail to process)
                let (dst_chunks, _) = as_arrays_mut::<8, _>(dst);
                let (a_chunks, _) = as_arrays::<16, _>(a);
                let (b_chunks, _) = as_arrays::<16, _>(b);
                for ((d, &a_chunk), &b_chunk) in dst_chunks.iter_mut().zip(a_chunks).zip(b_chunks) {
                    // Sign extend packed bytes into packed i16
                    let a_ext: i16x16 = cast(simd.avx2._mm256_cvtepi8_epi16(cast(a_chunk)));
                    let b_ext: i16x16 = cast(simd.avx2._mm256_cvtepi8_epi16(cast(b_chunk)));
                    // Perform the horizontal FMA, returning i32x8
                    let mag_a: i32x8 = cast(simd.avx2._mm256_madd_epi16(cast(a_ext), cast(a_ext)));
                    let mag_b: i32x8 = cast(simd.avx2._mm256_madd_epi16(cast(b_ext), cast(b_ext)));
                    // Sum to form stokes i
                    let stokes: i32x8 = cast(simd.avx2._mm256_add_epi32(cast(mag_a), cast(mag_b)));
                    // Convert to float
                    let floats: f32x8 = cast(simd.avx._mm256_cvtepi32_ps(cast(stokes)));
                    // Scale the fixed point result
                    let floats: [f32; 8] = cast(simd.avx._mm256_div_ps(cast(floats), scale));
                    // And assign
                    d.clone_from_slice(&floats);
                }
            }
        }

        simd.vectorize(Impl { simd, dst, a, b });
    } else {
        panic!("This hardware doesn't have support for x86_64_v3")
    }
}

pub fn stokes_i(out: &mut [f32; CHANNELS], pl: &Payload) {
    let a_slice = unsafe { std::mem::transmute::<&[Channel; 2048], &[i8; 4096]>(&pl.pol_a) };
    let b_slice = unsafe { std::mem::transmute::<&[Channel; 2048], &[i8; 4096]>(&pl.pol_b) };
    simd_stokes(out, a_slice, b_slice);
}
