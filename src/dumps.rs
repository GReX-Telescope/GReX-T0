//! Dumping voltage data

use crate::common::{Payload, BLOCK_TIMEOUT, CHANNELS, PACKET_CADENCE};
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

pub struct DumpRing {
    capacity: usize,
    container: Vec<Payload>,
    write_index: usize,
}

impl DumpRing {
    pub fn next_push(&mut self) -> &mut Payload {
        let before_idx = self.write_index;
        self.write_index = (self.write_index + 1) % self.capacity;
        &mut self.container[before_idx]
    }

    pub fn new(size_power: u32) -> Self {
        let cap = 2usize.pow(size_power);
        Self {
            container: vec![Payload::default(); cap],
            write_index: 0,
            capacity: cap,
        }
    }

    // Pack the ring into an array of [time, (pol_a, pol_b), channel, (re, im)]
    pub fn dump(&self, start_time: &Epoch, path: &Path, filename: &str) -> eyre::Result<()> {
        // Create a tmpfile for this dump, as that will be on the OS drive (probably)
        // Which should be faster storage than the result path

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

        // Fill times
        // Get the time of the first payload (the next write_index is the read index)
        let pl = self.container.get(self.write_index).unwrap();
        let mjd_start = pl.real_time(start_time).to_mjd_tai_days();
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

        // Write to the file, one timestep at a time, chunking in pols, channels, and reim
        voltages.set_chunking(&[1, 2, CHANNELS, 2])?;
        voltages.set_compression(1, true)?;
        let mut idx = 0;
        let mut read_idx = self.write_index;
        loop {
            let pl = self.container.get(read_idx).unwrap();
            voltages.put((idx, .., .., ..), pl.into_ndarray().view())?;
            idx += 1;
            read_idx = (read_idx + 1) % self.capacity;
            if read_idx == self.write_index {
                break;
            }
        }

        // Close the netcdf file
        drop(file);

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
    start_time: Epoch,
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
            match ring.dump(&start_time, &path, &filename) {
                Ok(_) => (),
                Err(e) => warn!("Error in dumping buffer - {}", e),
            }
        } else {
            // If we're not dumping, we're pushing data into the ringbuffer
            match payload_reciever.recv_ref_timeout(BLOCK_TIMEOUT) {
                Ok(pl) => {
                    let ring_ref = ring.next_push();
                    ring_ref.clone_from(&pl);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Closed) => break,
                Err(_) => unreachable!(),
            }
        }
    }
    Ok(())
}
