use crate::common::{
    processed_payload_start_time, Stokes, BLOCK_TIMEOUT, CHANNELS, PACKET_CADENCE,
};
use byte_slice_cast::AsByteSlice;
use eyre::eyre;
use hifitime::prelude::*;
use psrdada::prelude::*;
use sigproc_filterbank::write::WriteFilterbank;
use std::fs::File;
use std::path::Path;
use std::{collections::HashMap, io::Write, str::FromStr};
use thingbuf::mpsc::blocking::Receiver;
use thingbuf::mpsc::errors::RecvTimeoutError;
use tokio::sync::broadcast;
use tracing::{debug, info};

// Set by hardware (in MHz)
pub const HIGHBAND_MID_FREQ: f64 = 1529.93896484375; // Highend of band - half the channel spacing
pub const BANDWIDTH: f64 = 250.0;

/// Convert a chronno `DateTime` into a heimdall-compatible timestamp string
fn heimdall_timestamp(time: &Epoch) -> String {
    let fmt = Format::from_str("%Y-%m-%d-%H:%M:%S").unwrap();
    format!("{}", Formatter::new(*time, fmt))
}

/// A consumer that just grabs stokes off the channel and drops them
pub fn dummy_consumer(
    stokes_rcv: Receiver<Stokes>,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting dummy consumer");
    loop {
        if shutdown.try_recv().is_ok() {
            info!("Exfil task stopping");
            break;
        }
        match stokes_rcv.recv_ref_timeout(BLOCK_TIMEOUT) {
            Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Closed) => break,
            Err(_) => unreachable!(),
        }
    }
    Ok(())
}

pub fn dada_consumer(
    key: i32,
    stokes_rcv: Receiver<Stokes>,
    downsample_factor: usize,
    window_size: usize,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting DADA consumer");
    // DADA window
    let mut stokes_cnt = 0usize;
    // We will capture the timestamp on the first packet
    let mut first_payload = true;
    // Send the header (heimdall only wants one)
    let mut header = HashMap::from([
        ("NCHAN".to_owned(), CHANNELS.to_string()),
        ("BW".to_owned(), (-BANDWIDTH).to_string()),
        ("FREQ".to_owned(), "1405".to_owned()),
        ("NPOL".to_owned(), "1".to_owned()),
        ("NBIT".to_owned(), "32".to_owned()),
        ("OBS_OFFSET".to_owned(), 0.to_string()),
        (
            "TSAMP".to_owned(),
            (PACKET_CADENCE * downsample_factor as f64 * 1e6).to_string(),
        ),
    ]);
    // Grab PSRDADA writing context
    let mut client = HduClient::connect(key).expect("Could not connect to PSRDADA buffer");
    let (mut hc, mut dc) = client.split();
    let mut data_writer = dc
        .writer()
        .expect("Couldn't lock the DADA buffer for writing");
    info!("DADA header pushed, starting exfil to Heimdall");
    // Start the main consumer loop
    // FIXME FIXME How do we timeout of grabbing a dada block?
    loop {
        // Grab the next psrdada block we can write to (BLOCKING)
        let mut block = data_writer.next().unwrap();
        loop {
            if shutdown.try_recv().is_ok() {
                info!("Exfil task stopping");
                return Ok(());
            }
            // Grab the next stokes parameters (already downsampled)
            let mut stokes = stokes_rcv
                .recv_ref()
                .ok_or_else(|| eyre!("Channel closed"))?;
            debug_assert_eq!(stokes.len(), CHANNELS);
            // Timestamp first one
            if first_payload {
                first_payload = false;
                let time = processed_payload_start_time();
                let timestamp_str = heimdall_timestamp(&time);
                header.insert("UTC_START".to_owned(), timestamp_str);
                // Write the single header
                // Safety: All these header keys and values are valid
                unsafe { hc.write_header(&header).unwrap() };
            }
            // Zero the first and last 250 sample to remove the aliasing artifacts from the edges
            stokes[0..=250].fill(0.0);
            stokes[1797..=2047].fill(0.0);
            // Write the block
            block.write_all(stokes.as_byte_slice()).unwrap();
            // Increase our count
            stokes_cnt += 1;
            // If we've filled the window, commit it to PSRDADA
            if stokes_cnt == window_size {
                debug!("Committing window to PSRDADA");
                // Reset the stokes counter
                stokes_cnt = 0;
                // Commit data and update
                block.commit();
                //Break to finish the write
                break;
            }
        }
    }
}

/// Basically the same as the dada consumer, except write to a filterbank instead with no chunking
pub fn filterbank_consumer(
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
    fb.fch1 = Some(HIGHBAND_MID_FREQ); // End of band + half the step size
    fb.foff = Some(-(BANDWIDTH / CHANNELS as f64));
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
