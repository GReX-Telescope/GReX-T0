//! Task for injecting a fake pulse into the timestream to test/validate downstream components
use crate::common::{Payload, BLOCK_TIMEOUT, CHANNELS};
use byte_slice_cast::AsSliceOf;
use memmap2::Mmap;
use ndarray::{s, ArrayView, ArrayView2};
use std::{
    fs::File,
    path::PathBuf,
    time::{Duration, Instant},
};
use thingbuf::mpsc::{
    blocking::{StaticReceiver, StaticSender},
    errors::RecvTimeoutError,
};
use tokio::sync::broadcast;
use tracing::{info, warn};

fn read_pulse(pulse_mmap: &Mmap) -> eyre::Result<ArrayView2<f64>> {
    let floats = pulse_mmap[..].as_slice_of::<f64>()?;
    let time_samples = floats.len() / CHANNELS;
    let block = ArrayView::from_shape((CHANNELS, time_samples), floats)?;
    Ok(block)
}

pub fn pulse_injection_task(
    input: StaticReceiver<Payload>,
    output: StaticSender<Payload>,
    cadence: Duration,
    pulse_path: PathBuf,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    // Grab all the .dat files in the given directory
    let pulse_path = std::fs::read_dir(pulse_path);

    if let Ok(path) = pulse_path {
        let pulses: Vec<_> = path
            .filter_map(|f| match f {
                Ok(de) => {
                    let path = de.path();
                    let e = path.extension()?;
                    if e == "dat" {
                        Some(path)
                    } else {
                        None
                    }
                }
                Err(_) => None,
            })
            .collect();

        let mut pulse_cycle = pulses.iter().cycle();

        info!("Starting pulse injection!");

        let mut i = 0;
        let mut currently_injecting = false;
        let mut last_injection = Instant::now();

        // State for current pulse
        let mut current_mmap = unsafe { Mmap::map(&File::open(pulse_cycle.next().unwrap())?)? };
        let mut current_pulse = read_pulse(&current_mmap)?;

        loop {
            if shutdown.try_recv().is_ok() {
                info!("Injection task stopping");
                break;
            }
            // Grab payload from packet capture
            match input.recv_timeout(BLOCK_TIMEOUT) {
                Ok(mut payload) => {
                    if last_injection.elapsed() >= cadence {
                        last_injection = Instant::now();
                        currently_injecting = true;
                        i = 0;
                        info!("Injecting pulse!");
                    }
                    if currently_injecting {
                        // Get the slice of fake pulse data
                        let this_sample = current_pulse.slice(s![.., i]);
                        // Add the current time slice of the fake pulse into the stream of real data
                        // For both polarizations, add the real and imaginary part by the value of the corresponding channel in the fake pulse data
                        for (payload_val, pulse_val) in payload.pol_a.iter_mut().zip(this_sample) {
                            // Just add this pulse to the real component of the voltage
                            payload_val.0.re += *pulse_val as i8;
                        }
                        // And again for pol_b
                        for (payload_val, pulse_val) in payload.pol_b.iter_mut().zip(this_sample) {
                            payload_val.0.re += *pulse_val as i8;
                        }
                        i += 1;
                        // If we've gone through all of it, stop and move to the next pulse
                        if i == current_pulse.shape()[1] {
                            currently_injecting = false;
                            current_mmap =
                                unsafe { Mmap::map(&File::open(pulse_cycle.next().unwrap())?)? };
                            current_pulse = read_pulse(&current_mmap)?;
                        }
                    }
                    output.send(payload)?;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Closed) => break,
                Err(_) => unreachable!(),
            }
        }
    } else {
        // Missing the path, throw a warning and just connect the channels
        warn!("Pulse injection source folder missing, skipping pulse injection");
        loop {
            if shutdown.try_recv().is_ok() {
                info!("Injection task stopping");
                break;
            }
            match input.recv_timeout(BLOCK_TIMEOUT) {
                Ok(s) => output.send(s)?,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Closed) => break,
                Err(_) => unreachable!(),
            }
        }
    }
    Ok(())
}
