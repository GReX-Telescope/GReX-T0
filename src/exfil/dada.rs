use super::BANDWIDTH;
use crate::common::{processed_payload_start_time, Stokes, CHANNELS, PACKET_CADENCE};
use byte_slice_cast::AsByteSlice;
use eyre::eyre;
use hifitime::{
    efmt::{Format, Formatter},
    Epoch,
};
use psrdada::prelude::*;
use std::{collections::HashMap, io::Write, str::FromStr};
use thingbuf::mpsc::blocking::Receiver;
use tokio::sync::broadcast;
use tracing::{debug, info};

/// Convert a chronno `DateTime` into a heimdall-compatible timestamp string
fn heimdall_timestamp(time: &Epoch) -> String {
    let fmt = Format::from_str("%Y-%m-%d-%H:%M:%S").unwrap();
    format!("{}", Formatter::new(*time, fmt))
}

pub fn consumer(
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
            let stokes = stokes_rcv
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
