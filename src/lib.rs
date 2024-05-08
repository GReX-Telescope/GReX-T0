//! Separate library crate so we can run benchmarks

#![deny(clippy::all)]
//#![warn(clippy::pedantic)]

pub mod args;
pub mod calibrate;
pub mod capture;
pub mod common;
pub mod dumps;
pub mod exfil;
pub mod fpga;
pub mod injection;
pub mod monitoring;
pub mod processing;
pub mod telemetry;
