pub mod dada;
pub mod dummy;
pub mod filterbank;

// Set by hardware (in MHz)
pub const HIGHBAND_MID_FREQ: f64 = 1529.93896484375; // Highend of band - half the channel spacing
pub const BANDWIDTH: f64 = 250.0;
