//! Dumping voltage data

use crate::common::{payload_time, Payload, BLOCK_TIMEOUT, CHANNELS, FIRST_PACKET};
use crate::exfil::{BANDWIDTH, HIGHBAND_MID_FREQ};
use eyre::bail;
use ndarray::prelude::*;
use serde::Deserialize;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thingbuf::mpsc::{blocking::StaticReceiver, errors::RecvTimeoutError};
use tokio::{net::UdpSocket, sync::broadcast};
use tracing::{debug, error, info, warn};

// Just over 2 second window size (2^18)
const DUMP_SIZE: u64 = 262144;
const FILENAME_PREFIX: &str = "grex_dump";

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
    /// Last pushed payload count
    last: Option<u64>,
}

impl DumpRing {
    pub fn new(size_power: u32) -> Self {
        let capacity = 2usize.pow(size_power);
        // Because (linux) uses overcommited memory, this just asks the OS for the pages, it doesn't actually back this by RAM
        // This means we need to write actual values to every single slot to convince linux we're not dumb and we really really want like 100GB for our thread
        let mut buffer = Array::zeros((capacity, 2, CHANNELS, 2));
        // We're going to write a non-zero value to do something convincingly non-trivial
        // But this will be overwritten anyway
        buffer.fill(0xDEu8 as i8);
        Self {
            buffer,
            capacity,
            write_ptr: 0,
            full: false,
            oldest: None,
            last: None,
        }
    }

    /// Reset the ring buffer state (empty)
    pub fn reset(&mut self) {
        self.write_ptr = 0;
        self.full = false;
        self.oldest = None;
        self.last = None;
    }

    pub fn push(&mut self, pl: &Payload) {
        if let Some(last) = self.last {
            // Check to see if the incoming payload is monotonic
            if pl.count != last + 1 {
                error!(
                    count = pl.count,
                    last = last,
                    "Not monotonic, clearing buffer and starting over"
                );
                self.reset();
                return;
            } else {
                self.last = Some(pl.count);
            }
        }

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
            self.last = Some(pl.count);
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

    /// Write a subset of the ring to a netcdf file, erroring if OOB. Start and stop are inclusive.
    fn dump(&mut self, start_sample: u64, stop_sample: u64, path: &Path) -> eyre::Result<()> {
        // Fill times using the payload count of the oldest sample in the ring buffer
        if self.oldest.is_none() {
            warn!("Tried to dump an empty voltage buffer");
            // We didn't start to create a file, so we don't need to clean up one
            return Ok(());
        }

        let oldest = self.oldest.unwrap();
        let newest = oldest + (self.capacity as u64) - 1;

        debug!(
            "Attempting to dump {} to {}. Ring buffer covers {} to {} with the write ptr at {}",
            start_sample, stop_sample, oldest, newest, self.write_ptr
        );

        // The true dump size could have been modified by the caller to fit partial bursts into the window
        let this_dump_size = stop_sample - start_sample + 1;

        // Check bounds
        if start_sample < oldest
            || start_sample > newest
            || stop_sample < oldest
            || stop_sample > newest
            || start_sample > stop_sample
        {
            warn!("Requested samples out of bounds or out of order");
            return Ok(());
        }

        // Bounds are ok, create the file
        let mut file = netcdf::create(path)?;

        // Add the file dimensions
        file.add_dimension("time", this_dump_size as usize)?;
        file.add_dimension("pol", 2)?;
        file.add_dimension("freq", CHANNELS)?;
        file.add_dimension("reim", 2)?;

        // Describe the dimensions
        let mut mjd = file.add_variable::<f64>("time", &["time"])?;
        mjd.put_attribute("units", "Days")?;
        mjd.put_attribute("long_name", "TAI days since the MJD Epoch")?;

        let mjd_start = payload_time(start_sample).to_mjd_tai_days();
        let mjd_end = payload_time(stop_sample).to_mjd_tai_days();

        // And create the range
        let mjds = Array::linspace(mjd_start, mjd_end, this_dump_size as usize);
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
        // We want chunk sizes of 16MiB, which works out to 2048 time samples (less than our DUMP_SIZE)
        voltages.set_chunking(&[2048, 2, CHANNELS, 2])?;

        // Create two new consecutive views that are the subset of the ringbuffer we want to write,
        // covering the range [start_sample, stop_sample]

        let (a, b) = self.consecutive_views();
        let a_len = a.len_of(Axis(0));

        // There are three situations:
        // 1. The range is entirely in the first half
        if oldest as usize + a_len > stop_sample as usize {
            info!("burst is all in a");
            // Trim the chunk and write
            let start_idx = (start_sample - oldest) as usize;
            let stop_idx = (stop_sample - oldest) as usize;
            let slice = a.slice(s![start_idx..=stop_idx, .., .., ..]);
            voltages.put((..this_dump_size as usize, .., .., ..), slice)?;
        }
        // 2. The range is between the two chunks
        // Else branch implies that oldest + a_len <= stop_sample
        else if oldest as usize + a_len > start_sample as usize {
            info!("burst is between a and b");
            // stop idx for the first chunk is just the end of the chunk
            let start_idx = (start_sample - oldest) as usize;
            let a_slice = a.slice(s![start_idx.., .., .., ..]);
            voltages.put((..a_slice.len(), .., .., ..), a_slice)?;
            // start idx for the second chunk is the start of the chunk
            let stop_idx = stop_sample as usize - oldest as usize + a_len;
            let b_slice = b.slice(s![..=stop_idx, .., .., ..]);
            // Sanity check
            if a_slice.len() + b_slice.len() != this_dump_size as usize {
                error!(
                    "The size of the two slices doesn't match the total size we expected to dump"
                );
            }
            voltages.put((a_slice.len().., .., .., ..), b_slice)?;
        }
        // 3. The range is entirely in the second chunk
        // Else branch implies that oldest + a_len <= stop_sample && oldest + a_len <= start_sample
        else {
            info!("burst is all in b");
            let oldest_b = oldest as usize + a_len;
            let start_idx = start_sample as usize - oldest_b;
            let stop_idx = stop_sample as usize - oldest_b;
            let slice = b.slice(s![start_idx..=stop_idx, .., .., ..]);
            voltages.put((..this_dump_size as usize, .., .., ..), slice)?;
        }

        // Make sure the file is completley written to the disk
        file.sync()?;

        Ok(())
    }

    /// Pack a subset of the ring into an array of [time, (pol_a, pol_b), channel, (re, im)] and write to a file specified by the contents of the trigger message
    pub fn trigger_dump(
        &mut self,
        path: &Path,
        tm: TriggerMessage,
        downsample_factor: u32,
    ) -> eyre::Result<()> {
        // Goals: given tm.specnum, find the un-downsampled specnum in our block and write out a block centered at that point
        // As the ringbuffer will be in two segments, we need to deal with the possibility that the burst is across a ringbuffer boundary

        let filename = format!("{}-{}.nc", FILENAME_PREFIX, tm.candname);

        if let Some(oldest) = self.oldest {
            let newest = oldest + (self.capacity as u64) - 1;

            // However, the ring could be smaller than the chunk we plan to write out, in which case we're not going to bother finding the part that contains the pulse and just write the whole thing
            if self.capacity <= DUMP_SIZE as usize {
                warn!("Voltage buffer size smaller than preset dump size, dumping the whole thing");
                // Dump the whole thing
                self.dump(oldest, newest, &path.join(filename))?;
                return Ok(());
            }

            // Specnum is which spectrum heimdall found the pulse in.
            // So, the sample number of specnum 0 is the FIRST_PACKET that we processed and the sample number of specnum 1 is the downsample of samples FIRST_PACKET..=downsample_factor+FIRST_PACKET
            let true_sample =
                tm.itime as u64 * (downsample_factor as u64) + FIRST_PACKET.load(Ordering::Acquire);

            // Now find where in the block this sample lies (hopefully we didn't miss it, throwing an error if we did)
            // DUMP_SIZE is even, so we'll bias the sample one to the left
            let mut begin_sample = true_sample - DUMP_SIZE / 2 + 1;
            let mut end_sample = true_sample + DUMP_SIZE / 2;

            // Check if we totally missed the burst
            if oldest > end_sample {
                bail!("Ring buffer doesn't contain the requested sample, consider increasing the size of the buffer. The oldest sample in the buffer is {} and we wanted samples {}-{}", oldest, begin_sample, end_sample);
            }
            if newest < begin_sample {
                bail!("Ring buffer doesn't contain the requested sample, but strangely we wanted a sample from the future, this shouldn't happen");
            }

            // At this point we know at least part of the burst is in the buffer, now we need to check if it is trimmed by the edges
            if oldest > begin_sample {
                warn!("The dump block we would write is being cut off at the beginning, consider increasing the size of the buffer");
                begin_sample = oldest;
            }
            if newest < end_sample {
                warn!("The dump block we would write is being cut off at the end, consider increasing the size of the buffer");
                end_sample = newest;
            }
            // Now we have valid bounds of the block we can write
            self.dump(begin_sample, end_sample, &path.join(filename))
        } else {
            bail!("Tried to dump an empty ringbuffer")
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TriggerMessage {
    pub candname: String,
    pub itime: u32,
}

pub async fn trigger_task(
    sender: SyncSender<Vec<u8>>,
    port: u16,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting voltage ringbuffer trigger task!");
    // Create the socket
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let sock = UdpSocket::bind(addr).await?;
    let mut buf = vec![0; 128];
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
    signal_receiver: Receiver<Vec<u8>>,
    path: PathBuf,
    downsample_power: u32,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting voltage ringbuffer fill task!");
    loop {
        if shutdown.try_recv().is_ok() {
            info!("Dump task stopping");
            break;
        }
        // First check if we need to dump, as that takes priority
        if let Ok(bytes) = signal_receiver.try_recv() {
            // Parse to a string
            let tm_str = String::from_utf8(bytes);

            if let Ok(s) = tm_str {
                match serde_json::from_str::<TriggerMessage>(&s) {
                    Ok(tm) => {
                        // Send trigger to dump
                        info!("Dumping candidate {}", tm.candname);
                        match ring.trigger_dump(&path, tm, 2u32.pow(downsample_power)) {
                            Ok(_) => (),
                            Err(e) => warn!("Error in dumping buffer: {}", e),
                        }

                        // Clear the buffer, even if we errored
                        ring.reset();

                        // The dump may have taken a while, in which time the downstream task may have asked for *more* triggers
                        // This would imply that the signal_receiver could be full of stuff which would immediatly dump the next loop.
                        // To avoid this, we're going to clear out anything in that receiver now (which are triggers that occured during dumping)
                        let mut skipped_triggers = 0;
                        while signal_receiver.try_recv().is_ok() {
                            // Throw them out
                            skipped_triggers += 1;
                        }
                        if skipped_triggers > 0 {
                            warn!("We received {skipped_triggers} triggers to dump while we were dumping, these were skipped");
                        }

                        // We also need to clear out everything in the payload channel, because there will be a discontinuity
                        // in payload counts as we were dumping. Instead of just doing the backlog, might as well do an entire channel's worth.
                        // This will "lose" data, but is the conservative approach to making sure everything gets back to normal.
                        for _ in 0..(2 * payload_reciever.capacity()) {
                            match payload_reciever.recv_timeout(BLOCK_TIMEOUT) {
                                Ok(_) => {
                                    // Do nothing
                                }
                                Err(RecvTimeoutError::Timeout) => continue,
                                Err(RecvTimeoutError::Closed) => return Ok(()),
                                Err(_) => unreachable!(),
                            }
                        }

                        // Keep on loopin
                        continue;
                    }
                    Err(e) => {
                        warn!("Error deserializing JSON trigger message - {}", e);
                    }
                }
            } else {
                warn!("Trigger message contained invalid UTF8");
            }
        } else {
            // If we're not dumping, we're pushing data into the ringbuffer
            match payload_reciever.recv_timeout(BLOCK_TIMEOUT) {
                Ok(pl) => {
                    ring.push(&pl);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Closed) => return Ok(()),
                Err(_) => unreachable!(),
            }
        }
    }
    Ok(())
}
