use crate::common::{
    processed_payload_start_time, Stokes, BLOCK_TIMEOUT, CHANNELS, PACKET_CADENCE,
};
use hifitime::prelude::*;
use sigproc_filterbank::write::WriteFilterbank;
use std::fs::File;
use std::path::Path;
use std::{io::Write, str::FromStr};
use thingbuf::mpsc::blocking::Receiver;
use thingbuf::mpsc::errors::RecvTimeoutError;
use tokio::sync::broadcast;
use tracing::info;

/// Basically the same as the dada consumer, except write to a filterbank instead with no chunking
pub fn consumer(
    stokes_rcv: Receiver<Stokes>,
    downsample_factor: usize,
    path: &Path,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting filterbank consumer");
    // Filename with ISO 8610 standard format
    let fmt = Format::from_str("%Y%m%dT%H%M%S").unwrap();
    let filename = format!("grex-{}.fil", Formatter::new(Epoch::now()?, fmt));
    let file_path = path.join(filename);
    // Create the file
    let mut file = File::create(file_path)?;
    // Create the filterbank context
    let mut fb = WriteFilterbank::new(CHANNELS, 1);
    // Setup the header stuff
    fb.fch1 = Some(super::HIGHBAND_MID_FREQ); // End of band + half the step size
    fb.foff = Some(-(super::BANDWIDTH / CHANNELS as f64));
    fb.tsamp = Some(PACKET_CADENCE * downsample_factor as f64);
    // We will capture the timestamp on the first packet
    let mut first_payload = true;
    loop {
        if shutdown.try_recv().is_ok() {
            info!("Exfil task stopping");
            break;
        }
        // Grab next stokes
        match stokes_rcv.recv_ref_timeout(BLOCK_TIMEOUT) {
            Ok(stokes) => {
                // Timestamp first one
                if first_payload {
                    first_payload = false;
                    let time = processed_payload_start_time();
                    fb.tstart = Some(time.to_mjd_tai_days());
                    // Write out the header
                    file.write_all(&fb.header_bytes()).unwrap();
                }
                // Stream to FB
                file.write_all(&fb.pack(&stokes))?;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Closed) => break,
            Err(_) => unreachable!(),
        }
    }
    Ok(())
}
