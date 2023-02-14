use crate::capture::FIRST_PACKET;
use crate::common::{Stokes, CHANNELS, PACKET_CADENCE};
use anyhow::anyhow;
use byte_slice_cast::AsByteSlice;
use hifitime::prelude::*;
use lending_iterator::prelude::*;
use log::{debug, info};
use psrdada::client::DadaClient;
use sigproc_filterbank::write::WriteFilterbank;
use std::{collections::HashMap, io::Write, str::FromStr, sync::atomic::Ordering};
use thingbuf::mpsc::blocking::Receiver;

// Set by hardware (in MHz)
const LOWBAND_MID_FREQ: f64 = 1_280.061_035_16;
const BANDWIDTH: f64 = 250.0;

/// Convert a chronno `DateTime` into a heimdall-compatible timestamp string
fn heimdall_timestamp(time: &Epoch) -> String {
    let fmt = Format::from_str("%Y-%m-%d-%H:%M:%S").unwrap();
    format!("{}", Formatter::new(*time, fmt))
}

/// A consumer that just grabs stokes off the channel and drops them
pub fn dummy_consumer(stokes_rcv: Receiver<Stokes>) -> anyhow::Result<()> {
    info!("Starting dummy consumer");
    while stokes_rcv.recv().is_some() {}
    Ok(())
}

pub fn dada_consumer(
    key: i32,
    stokes_rcv: Receiver<Stokes>,
    payload_start: Epoch,
    downsample_factor: usize,
    window_size: usize,
) -> anyhow::Result<()> {
    // DADA window
    let mut stokes_cnt = 0usize;
    // We will capture the timestamp on the first packet
    let mut first_payload = true;
    // Send the header (heimdall only wants one)
    let mut header = HashMap::from([
        ("NCHAN".to_owned(), CHANNELS.to_string()),
        ("BW".to_owned(), BANDWIDTH.to_string()),
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
    let mut client = DadaClient::new(key).expect("Could not connect to PSRDADA buffer");
    let (mut hc, mut dc) = client.split();
    let mut data_writer = dc.writer();
    info!("DADA header pushed, starting exfil to Heimdall");
    // Start the main consumer loop
    loop {
        // Grab the next psrdada block we can write to (BLOCKING)
        let mut block = data_writer.next().unwrap();
        loop {
            // Grab the next stokes parameters (already downsampled)
            let mut stokes = stokes_rcv
                .recv_ref()
                .ok_or_else(|| anyhow!("Channel closed"))?;
            debug_assert_eq!(stokes.len(), CHANNELS);
            // Timestamp first one
            if first_payload {
                first_payload = false;
                // The first payload we recieve will be payload #1 (as we armed and triggered)
                // We'll compute the timestamp via the first payload count and the cadence
                let first_payload_time = payload_start
                    + (PACKET_CADENCE * FIRST_PACKET.load(Ordering::Acquire) as f64).seconds();
                let timestamp_str = heimdall_timestamp(&first_payload_time);
                header.insert("UTC_START".to_owned(), timestamp_str);
                // Write the single header
                // Safety: All these header keys and values are valid
                unsafe { hc.push_header(&header).unwrap() };
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
                debug!("Commiting window to PSRDADA");
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
    payload_start: Epoch,
    downsample_factor: usize,
) -> anyhow::Result<()> {
    // Filename with ISO 8610 standard format
    let fmt = Format::from_str("%Y%m%dT%H%M%S").unwrap();
    let filename = format!("grex-{}.fil", Formatter::new(Epoch::now()?, fmt));
    // Create the file
    let mut file = std::fs::File::create(filename)?;
    // Create the filterbank context
    let mut fb = WriteFilterbank::new(CHANNELS, 1);
    // Setup the header stuff
    fb.fch1 = Some(LOWBAND_MID_FREQ); // Start of band + half the step size
    fb.foff = Some(BANDWIDTH / CHANNELS as f64);
    fb.tsamp = Some(PACKET_CADENCE * downsample_factor as f64);
    // We will capture the timestamp on the first packet
    let mut first_payload = true;
    loop {
        // Grab next stokes
        let stokes = stokes_rcv
            .recv_ref()
            .ok_or_else(|| anyhow!("Channel closed"))?;
        // Timestamp first one
        if first_payload {
            first_payload = false;
            let first_payload_time = payload_start
                + (PACKET_CADENCE * FIRST_PACKET.load(Ordering::Acquire) as f64).seconds();
            fb.tstart = Some(first_payload_time.to_mjd_utc_days());
            // Write out the header
            file.write_all(&fb.header_bytes()).unwrap();
        }
        // Stream to FB
        file.write_all(&fb.pack(&stokes))?;
    }
}
