//! Dumping voltage data

use crate::common::{payload_time, Payload, BLOCK_TIMEOUT, CHANNELS, PACKET_CADENCE};
use crate::exfil::{BANDWIDTH, HIGHBAND_MID_FREQ};
use hifitime::prelude::*;
use ndarray::prelude::*;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};
use thingbuf::mpsc::{
    blocking::{Receiver, Sender, StaticReceiver},
    errors::RecvTimeoutError,
};
use tokio::{net::UdpSocket, sync::broadcast};
use tracing::{info, warn};

/// The voltage dump ringbuffer
#[derive(Debug)]
pub struct DumpRing {
    /// The next time index we write into
    write_ptr: usize,
    /// The data itself (heap allocated)
    buffer: Array4<i8>,
    /// The number of time samples in this array
    capacity: usize,
    /// The timestamp (packet count) of the oldest sample (pointed to by read_ptr).
    /// None if the buffer is empty
    oldest: Option<u64>,
    // If the buffer is completly full
    full: bool,
}

impl DumpRing {
    pub fn new(size_power: u32) -> Self {
        let capacity = 2usize.pow(size_power);
        // Allocate all the memory for the array (heap)
        let buffer = Array::zeros((capacity, 2, CHANNELS, 2));
        Self {
            buffer,
            capacity,
            write_ptr: 0,
            full: false,
            oldest: None,
        }
    }

    pub fn push(&mut self, pl: &Payload) {
        // Copy the data into the slice pointed to by the write_ptr
        let data_view = pl.as_ndarray_data_view();
        self.buffer
            .slice_mut(s![self.write_ptr, .., .., ..])
            .assign(&data_view);

        // Move the pointer
        self.write_ptr = (self.write_ptr + 1) % self.capacity;
        // If there was no data update the timeslot of the oldest data and increment the write_ptr
        if self.oldest.is_none() {
            self.oldest = Some(pl.count);
            // Nothing left to do
            return;
        }

        // If we're full, we overwrite old data
        // which increments the payload count of old data by one
        // as they are always monotonically increasing by one
        if self.full {
            self.oldest = Some(self.oldest.unwrap() + 1);
        }

        // If we wrapped around the first time, we are now full
        if self.write_ptr == 0 && !self.full {
            self.full = true;
        }
    }

    /// Get the two array views that represent the time-ordered, consecutive memory chunks of the ringbuffer.
    /// The first view will always have data in it, and the second view will be buffer_capacity - length(first_view)
    fn consecutive_views(&self) -> (ArrayView4<i8>, ArrayView4<i8>) {
        // There are four different cases
        // 1. the buffer is empty or
        // 2. The buffer has yet to be filled to capacity  (and we always start at index 0) so there's only really one chunk
        if !self.full {
            (
                self.buffer.slice(s![..self.write_ptr, .., .., ..]),
                ArrayView4::from_shape((0, 2, CHANNELS, 2), &[]).unwrap(),
            )
        } else {
            // 3. The buffer is full and the write_ptr is at 0 (so the buffer is in order) or
            // 4. The write_ptr is non zero and the buffer is full, meaning the write_ptr is the split where data at its value to the end is the oldest chunk
            (
                self.buffer.slice(s![self.write_ptr.., .., .., ..]),
                self.buffer.slice(s![..self.write_ptr, .., .., ..]),
            )
        }
    }

    // Pack the ring into an array of [time, (pol_a, pol_b), channel, (re, im)]
    pub fn dump(&self, path: &Path, filename: &str) -> eyre::Result<()> {
        // Create a tmpfile for this dump, as that will be on the OS drive (probably),
        // which should be faster storage than the result path
        let tmp_path = std::env::temp_dir();
        let tmp_file_path = tmp_path.join(filename);
        let mut file = netcdf::create(tmp_file_path.clone())?;

        // Add the file dimensions
        file.add_dimension("time", self.capacity)?;
        file.add_dimension("pol", 2)?;
        file.add_dimension("freq", CHANNELS)?;
        file.add_dimension("reim", 2)?;

        // Describe the dimensions
        let mut mjd = file.add_variable::<f64>("time", &["time"])?;
        mjd.put_attribute("units", "Days")?;
        mjd.put_attribute("long_name", "TAI days since the MJD Epoch")?;

        // Fill times using the payload count of the oldest sample in the ring buffer
        let mjd_start = payload_time(self.oldest.unwrap()).to_mjd_tai_days();
        let mjd_end = mjd_start + self.capacity as f64 * PACKET_CADENCE / 86400f64; // candence in days

        // And create the range
        let mjds = Array::linspace(mjd_start, mjd_end, self.capacity);
        mjd.put(.., mjds.view())?;

        let mut pol = file.add_string_variable("pol", &["pol"])?;
        pol.put_attribute("long_name", "Polarization")?;
        pol.put_string("a", 0)?;
        pol.put_string("b", 1)?;

        let mut freq = file.add_variable::<f64>("freq", &["freq"])?;
        freq.put_attribute("units", "Megahertz")?;
        freq.put_attribute("long_name", "Frequency")?;
        let freqs = Array::linspace(HIGHBAND_MID_FREQ, HIGHBAND_MID_FREQ - BANDWIDTH, CHANNELS);
        freq.put(.., freqs.view())?;

        let mut reim = file.add_string_variable("reim", &["reim"])?;
        reim.put_attribute("long_name", "Complex")?;
        reim.put_string("real", 0)?;
        reim.put_string("imaginary", 1)?;

        // Setup our data block
        let mut voltages = file.add_variable::<i8>("voltages", &["time", "pol", "freq", "reim"])?;
        voltages.put_attribute("long_name", "Channelized Voltages")?;
        voltages.put_attribute("units", "Volts")?;

        // Write to the file, one timestep at a time (chunking in pols, channels, and reim)
        // We want chunk sizes of 16MiB, which works out to 2048 time samples
        voltages.set_chunking(&[2048, 2, CHANNELS, 2])?;

        let (a, b) = self.consecutive_views();
        let a_len = a.len_of(Axis(0));
        voltages.put((..a_len, .., .., ..), a)?;
        voltages.put((a_len.., .., .., ..), b)?;

        // Finally, spawn (and detatch) a thread to move this file to the actual requested final spot on the disk
        // Due to https://github.com/rust-lang/rustup/issues/1239, this has to be a copy then delete instead of a move

        // If the final path is the same as the tmp path (as in we're dumping to tmp anyway)
        // No need to do this
        let final_file_path = path.join(filename);
        if final_file_path != tmp_file_path {
            let _ = std::thread::spawn(move || {
                std::fs::copy(tmp_file_path.clone(), final_file_path).expect("Couldn't move file");
                std::fs::remove_file(tmp_file_path).expect("Couldn't remove tmp file");
            });
        }

        Ok(())
    }
}

pub async fn trigger_task(
    sender: Sender<Vec<u8>>,
    port: u16,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting voltage ringbuffer trigger task!");
    // Create the socket
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let sock = UdpSocket::bind(addr).await?;
    // We expect something like a UUIDv4 string, which is 36 characters, round up to 64 seems fine
    let mut buf = [0; 64];
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!("Voltage ringbuffer trigger task stopping");
                break;
            }
            // Receive bytes from the socket, optionally containing a file suffix
            // And send to the dump task
            res = sock.recv_from(&mut buf) => {
                let (n,_) = res.expect("Failed to recv_from trigger socket");
                sender.send(buf[..n].to_vec())?;
            }
        }
    }
    Ok(())
}

pub fn dump_task(
    mut ring: DumpRing,
    payload_reciever: StaticReceiver<Payload>,
    signal_reciever: Receiver<Vec<u8>>,
    path: PathBuf,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting voltage ringbuffer fill task!");
    // Create timestamp format for fallback filename
    // Filename with ISO 8610 standard format
    let fmt = Format::from_str("%Y%m%dT%H%M%S").unwrap();
    loop {
        if shutdown.try_recv().is_ok() {
            info!("Dump task stopping");
            break;
        }
        // First check if we need to dump, as that takes priority
        if let Ok(bytes) = signal_reciever.try_recv() {
            let mut filename_suffix = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    warn!(
                        "Incoming voltage dump trigger filename invalid, falling back to timestamp"
                    );
                    format!("{}", Formatter::new(Epoch::now()?, fmt))
                }
            };
            // Don't write whitespace
            if filename_suffix.is_empty() || filename_suffix.chars().all(|c| c.is_whitespace()) {
                warn!("Incoming voltage dump trigger filename was empty or all whitespace, falling back to timestamp");
                filename_suffix = format!("{}", Formatter::new(Epoch::now()?, fmt));
            }
            let filename = format!("grex_dump-{}.nc", filename_suffix);
            info!("Dumping ringbuffer to file: {}", filename);
            match ring.dump(&path, &filename) {
                Ok(_) => (),
                Err(e) => warn!("Error in dumping buffer - {}", e),
            }
        } else {
            // If we're not dumping, we're pushing data into the ringbuffer
            match payload_reciever.recv_timeout(BLOCK_TIMEOUT) {
                Ok(pl) => {
                    ring.push(&pl);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Closed) => break,
                Err(_) => unreachable!(),
            }
        }
    }
    Ok(())
}
