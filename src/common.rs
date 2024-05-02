//! Common types shared between tasks

use arrayvec::ArrayVec;
use hifitime::prelude::*;
use ndarray::prelude::*;
use num_complex::Complex;
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
        unsafe { ArrayView::from_shape_ptr((2, CHANNELS, 2), std::mem::transmute(raw_ptr)) }
    }
}
