//! Common types shared between tasks

use arrayvec::ArrayVec;
use hifitime::prelude::*;
use ndarray::{s, Array3, ArrayView};
use num_complex::Complex;

/// Number of frequency channels (set by gateware)
pub const CHANNELS: usize = 2048;
/// How sure are we?
pub const PACKET_CADENCE: f64 = 8.192e-6;
/// Standard timeout for blocking ops
pub const BLOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub type Stokes = ArrayVec<f32, CHANNELS>;

/// The complex number representing the voltage of a single channel
#[derive(Debug, Clone, Copy)]
pub struct Channel(pub Complex<i8>);

impl Channel {
    pub fn new(re: i8, im: i8) -> Self {
        Self(Complex::new(re, im))
    }

    pub fn abs_squared(&self) -> u16 {
        let r = i16::from(self.0.re);
        let i = i16::from(self.0.im);
        (r * r + i * i) as u16
    }
}

pub type Channels = [Channel; CHANNELS];

pub fn stokes_i(a: &Channels, b: &Channels) -> Stokes {
    // This allocated uninit, so we gucci
    let mut stokes = ArrayVec::new();
    for (a, b) in a.iter().zip(b) {
        // Source is Fix8_7, so x^2 is Fix16_14, sum won't have bit growth
        stokes.push(f32::from(a.abs_squared() + b.abs_squared()) / f32::from(1u16 << 14));
    }
    stokes
}

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
    /// Calculate the Stokes-I parameter for this payload
    pub fn stokes_i(&self) -> Stokes {
        stokes_i(&self.pol_a, &self.pol_b)
    }

    pub fn packed_pols(&self) -> (&[i8], &[i8]) {
        // # Safety
        // - Data is valid for reads of len as each pol has exactly CHANNELS * 2 bytes
        // - and is contiguous as per the spec of Complex<i8>
        // - Data is initialized at this point as Self has been constructed
        // - Data will not be mutated as this function takes an immutable borrow
        // - Total len is smaller than isize::MAX
        let bytes_a = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(self.pol_a).cast::<i8>(),
                CHANNELS * 2, // Real + Im for each element
            )
        };
        let bytes_b = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(self.pol_b).cast::<i8>(),
                CHANNELS * 2, // Real + Im for each element
            )
        };
        (bytes_a, bytes_b)
    }

    // ndarray of size [(pol_a,pol_b), CHANNELS, (re,im)]
    pub fn into_ndarray(&self) -> Array3<i8> {
        let mut buf = Array3::zeros((2, CHANNELS, 2));
        let (a, b) = self.packed_pols();
        let a = ArrayView::from_shape((CHANNELS, 2), a).expect("Failed to make array view");
        let b = ArrayView::from_shape((CHANNELS, 2), b).expect("Failed to make array view");
        // And assign
        buf.slice_mut(s![0, .., ..]).assign(&a);
        buf.slice_mut(s![1, .., ..]).assign(&b);
        buf
    }

    /// Return the real UTC time of this packet
    pub fn real_time(&self, start_time: &Epoch) -> Epoch {
        let second_offset = (self.count as f64 * PACKET_CADENCE).seconds();
        *start_time + second_offset
    }
}
