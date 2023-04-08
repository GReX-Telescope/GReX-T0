//! Task for injecting a fake pulse into the timestream to test/validate downstream components

use crate::common::{Stokes, CHANNELS};
use byte_slice_cast::AsSliceOf;
use crossbeam_channel::{Receiver, Sender};
use log::info;
use memmap2::Mmap;
use ndarray::{s, ArrayView, ArrayView2};
use rand_distr::{Distribution, Normal};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn read_pulse(pulse_mmap: &Mmap) -> anyhow::Result<ArrayView2<f64>> {
    let floats = pulse_mmap[..].as_slice_of::<f64>()?;
    let time_samples = floats.len() / CHANNELS;
    let block = ArrayView::from_shape((CHANNELS, time_samples), floats)?;
    Ok(block)
}

pub fn pulse_injection_task(
    input: Receiver<Stokes>,
    output: Sender<Stokes>,
    cadence: Duration,
    pulse_path: PathBuf,
) -> anyhow::Result<()> {
    // Grab all the .dat files in the given directory
    let pulses: Vec<_> = std::fs::read_dir(pulse_path)?
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

    // FIXME remove
    let normal = Normal::new(0.4, 0.05).unwrap();

    loop {
        // Grab stokes from downsample
        let mut s = input.recv()?;
        if last_injection.elapsed() >= cadence {
            last_injection = Instant::now();
            currently_injecting = true;
            i = 0;
        }
        if currently_injecting {
            // Get the slice of fake pulse data
            let this_sample = current_pulse.slice(s![.., i]);
            // Add the current time slice of the fake pulse into the stream of real data
            for (i, source) in s.iter_mut().zip(this_sample) {
                *i = *source as f32 + normal.sample(&mut rand::thread_rng());
            }
            i += 1;
            // If we've gone through all of it, stop and move to the next pulse
            if i == current_pulse.shape()[1] {
                currently_injecting = false;
                current_mmap = unsafe { Mmap::map(&File::open(pulse_cycle.next().unwrap())?)? };
                current_pulse = read_pulse(&current_mmap)?;
            }
        }
        output.send(s.clone())?;
    }
}
