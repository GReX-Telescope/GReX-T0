//! Task for injecting a fake pulse into the timestream to test/validate downstream components
use crate::{
    common::{payload_time, Channel, Payload, BLOCK_TIMEOUT, CHANNELS, FIRST_PACKET},
    db::InjectionRecord,
};
use byte_slice_cast::AsSliceOf;
use memmap2::Mmap;
use ndarray::{s, Array2, ArrayView, ArrayView2};
use pulp::{as_arrays, as_arrays_mut, cast, x86::V3};
use std::{
    fs::File,
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use thingbuf::mpsc::{
    blocking::{StaticReceiver, StaticSender},
    errors::RecvTimeoutError,
};
use tokio::sync::broadcast;
use tracing::info;
use eyre::eyre;

fn read_pulse(pulse_mmap: &Mmap) -> eyre::Result<ArrayView2<i8>> {
    let raw_bytes = pulse_mmap[..].as_slice_of::<i8>()?;
    let time_samples = raw_bytes.len() / CHANNELS;
    let block = ArrayView::from_shape((time_samples, CHANNELS), raw_bytes)?;
    Ok(block)
}

pub struct Injections {
    pulses: Vec<(String, Array2<i8>)>,
}

impl Injections {
    pub fn new(pulse_path: PathBuf) -> eyre::Result<Self> {
        // Grab all the .dat files in the given directory
        let pulse_files: Vec<_> = std::fs::read_dir(pulse_path)?
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

        // This could be empty
        if pulse_files.is_empty() {
            return Err(eyre!("No pulses to inject"))
        }

        // Read all the pulses off the disk
        let mut pulses = vec![];
        for file in pulse_files {
            let filename = file
                .file_name()
                .expect("Invalid file name")
                .to_string_lossy()
                .into();
            let mmap = unsafe { Mmap::map(&File::open(file)?)? };
            let pulse_view = read_pulse(&mmap)?;
            pulses.push((filename, pulse_view.to_owned()));
        }

        Ok(Self { pulses })
    }
}

pub fn simd_injection(live: &mut [i8; 2 * CHANNELS], injection: &[i8; CHANNELS]) {
    if let Some(simd) = V3::try_new() {
        struct Impl<'a> {
            simd: V3,
            dst: &'a mut [i8],
            src: &'a [i8],
        }

        impl pulp::NullaryFnOnce for Impl<'_> {
            type Output = ();

            #[inline(always)]
            fn call(self) -> Self::Output {
                let Self { simd, src, dst } = self;

                // Zeros to interleave
                let zeros = cast(simd.splat_i8x32(0));
                // Chunks to line up with AVX256
                let (src_chunks, _) = as_arrays::<16, _>(src);
                let (dst_chunks, _) = as_arrays_mut::<32, _>(dst);
                for (d, &s) in dst_chunks.iter_mut().zip(src_chunks) {
                    // Cast the source slice into a 256-bit lane (noop)
                    let s = simd.avx._mm256_castsi128_si256(cast(s));
                    // Unpack and interleave the lower bytes
                    let res_lo = simd.avx2._mm256_unpacklo_epi8(s, zeros);
                    // Unpack and interleave the higher bytes
                    let res_hi = simd.avx2._mm256_unpackhi_epi8(s, zeros);
                    // Concat the lower and upper to interleave
                    let interleaved = simd.avx2._mm256_permute2x128_si256::<0x20>(res_lo, res_hi);
                    // Perform the add
                    let res: [i8; 32] = cast(simd.avx2._mm256_add_epi8(cast(*d), interleaved));
                    // And assign
                    d.clone_from_slice(&res);
                }
                // No tail to process as both are multiples of 16
            }
        }

        simd.vectorize(Impl {
            simd,
            dst: live,
            src: injection,
        })
    } else {
        panic!("This hardware doesn't have support for x86_64_v3")
    }
}

/// Inject this pulse sample into the given payload
pub fn inject(pl: &mut Payload, sample: &[i8; CHANNELS]) {
    // Safety: These transmutes are safe because Complex<i8> has the same alignment requirements as an i8
    let a_slice =
        unsafe { std::mem::transmute::<&mut [Channel; 2048], &mut [i8; 4096]>(&mut pl.pol_a) };
    let b_slice =
        unsafe { std::mem::transmute::<&mut [Channel; 2048], &mut [i8; 4096]>(&mut pl.pol_b) };
    simd_injection(a_slice, sample);
    simd_injection(b_slice, sample);
}

pub fn pulse_injection_task(
    input: StaticReceiver<Payload>,
    output: StaticSender<Payload>,
    injection_record_sender: std::sync::mpsc::SyncSender<InjectionRecord>,
    cadence: Duration,
    injections: Injections,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting pulse injection!");

    // State variables
    let mut pulse_cycle = injections.pulses.iter().cycle();
    let mut i = 0;
    let mut currently_injecting = false;
    let mut last_injection = Instant::now();
    let mut this_pulse = pulse_cycle.next().unwrap();

    let current_pulse_length = this_pulse.1.shape()[0];

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
                    let record = InjectionRecord {
                        mjd: payload_time(payload.count).to_mjd_tai_days(),
                        sample: payload.count - FIRST_PACKET.load(Ordering::Acquire),
                        filename: this_pulse.0.clone(),
                    };
                    info!(
                        filename = record.filename,
                        mjd = record.mjd,
                        "Injecting pulse"
                    );
                    let _ = injection_record_sender.send(record);
                }
                if currently_injecting {
                    // Get the slice of fake pulse data and inject
                    inject(
                        &mut payload,
                        this_pulse
                            .1
                            .slice(s![i, ..])
                            .as_slice()
                            .expect("Sliced injection not in correct memory order")
                            .try_into()
                            .expect("Wrong number of channels"),
                    );
                    i += 1;
                    // If we've gone through all of it, stop and move to the next pulse
                    if i == current_pulse_length {
                        currently_injecting = false;
                        this_pulse = pulse_cycle.next().unwrap();
                    }
                }
                output.send(payload)?;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Closed) => break,
            Err(_) => unreachable!(),
        }
    }
    Ok(())
}
