//! Dumping voltage data

use crate::common::{Payload, BLOCK_TIMEOUT, CHANNELS};
use hdf5::File;
use hifitime::prelude::*;
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
        self.write_index = (self.write_index + 1) & (self.capacity - 1);
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
    pub fn dump(&self, start_time: &Epoch, path: &Path) -> eyre::Result<()> {
        // Filename with ISO 8610 standard format
        let fmt = Format::from_str("%Y%m%dT%H%M%S").unwrap();
        let filename = format!("grex_dump-{}.h5", Formatter::new(Epoch::now()?, fmt));
        let file_path = path.join(filename);
        let file = File::create(file_path)?;
        let ds = file
            .new_dataset::<i8>()
            .chunk((1, 2, CHANNELS, 2))
            .shape((self.container.len(), 2, CHANNELS, 2))
            .create("voltages")?;
        // And then write in chunks, draining the buffer
        let mut idx = 0;
        let mut payload_time;
        let mut read_idx = self.write_index;
        loop {
            let pl = self.container[read_idx];
            ds.write_slice(&pl.into_ndarray(), (idx, .., .., ..))?;
            payload_time = pl.real_time(start_time);
            idx += 1;
            // Increment read_index, mod the size
            read_idx = (read_idx + 1) & (self.capacity - 1);
            // Check if we've gone all the way around
            if read_idx == self.write_index {
                break;
            }
        }
        // Set the time attribute
        let attr = ds.new_attr::<i64>().create("timestamp")?;
        attr.write_scalar(&payload_time.to_mjd_utc_days())?;
        Ok(())
    }
}

pub async fn trigger_task(
    sender: Sender<()>,
    port: u16,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting voltage ringbuffer trigger task!");
    // Create the socket
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let sock = UdpSocket::bind(addr).await?;
    // Maybe even 0 would work, we don't expect data
    let mut buf = [0; 10];
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!("Voltage ringbuffer trigger task stopping");
                break;
            }
            _ = sock.recv_from(&mut buf) => {
                sender.send(())?;
            }
        }
    }
    Ok(())
}

pub fn dump_task(
    mut ring: DumpRing,
    payload_reciever: StaticReceiver<Payload>,
    signal_reciever: Receiver<()>,
    start_time: Epoch,
    path: PathBuf,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting voltage ringbuffer fill task!");
    loop {
        if shutdown.try_recv().is_ok() {
            info!("Dump task stopping");
            break;
        }
        // First check if we need to dump, as that takes priority
        if signal_reciever.try_recv().is_ok() {
            info!("Dumping ringbuffer");
            match ring.dump(&start_time, &path) {
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
