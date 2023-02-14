//! Logic for capturing raw packets from the NIC, parsing them into payloads, and sending them to other processing threads

use crate::common::{Channel, Payload};
use anyhow::anyhow;
use arrayvec::ArrayVec;
use log::{error, info, warn};
use socket2::{Domain, Socket, Type};
use std::net::UdpSocket;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use thingbuf::mpsc::blocking::{Receiver, Sender};

/// FPGA UDP "Word" size (8 bytes as per CASPER docs)
const WORD_SIZE: usize = 8;
/// Size of the packet count header
const TIMESTAMP_SIZE: usize = 8;
/// Total number of bytes in the spectra block of the UDP payload
const SPECTRA_SIZE: usize = 8192;
/// Total UDP payload size
pub const PAYLOAD_SIZE: usize = SPECTRA_SIZE + TIMESTAMP_SIZE;
/// Maximum number of payloads we want in the backlog
const BACKLOG_BUFFER_PAYLOADS: usize = 1024;
/// Polling interval for stats
const STATS_POLL_DURATION: Duration = Duration::from_secs(10);
/// Global atomic to hold the count of the first packet
pub static FIRST_PACKET: AtomicU64 = AtomicU64::new(0);

impl Payload {
    /// Construct a payload instance from a raw UDP payload
    #[allow(clippy::cast_possible_wrap)]
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        // Size hint
        debug_assert_eq!(bytes.len(), PAYLOAD_SIZE);
        let mut payload = Payload::default();
        for (i, word) in bytes[TIMESTAMP_SIZE..].chunks_exact(WORD_SIZE).enumerate() {
            // Each word contains two frequencies for each polarization
            // [A1 B1 A2 B2]
            // Where each channel is [Re Im] as FixedI8<7>
            payload.pol_a[2 * i] = Channel::new(word[0] as i8, word[1] as i8);
            payload.pol_a[2 * i + 1] = Channel::new(word[4] as i8, word[5] as i8);
            payload.pol_b[2 * i] = Channel::new(word[2] as i8, word[3] as i8);
            payload.pol_b[2 * i + 1] = Channel::new(word[6] as i8, word[7] as i8);
        }
        // Then unpack the timestamp/order
        payload.count = u64::from_be_bytes(
            bytes[0..TIMESTAMP_SIZE]
                .try_into()
                .expect("This is exactly 8 bytes"),
        );
        payload
    }
}

#[derive(thiserror::Error, Debug)]
/// Errors that can be produced from captures
pub enum Error {
    #[error("We recieved a payload which wasn't the size we expected {0}")]
    SizeMismatch(usize),
    #[error("Failed to set the recv buffer size. We tried to set {expected}, but found {found}. Check sysctl net.core.rmem_max")]
    SetRecvBufferFailed { expected: usize, found: usize },
}

type Count = u64;
pub type PayloadBytes = [u8; PAYLOAD_SIZE];

pub struct Capture {
    sock: UdpSocket,
    pub backlog: HashMap<Count, PayloadBytes>,
    pub drops: usize,
    pub processed: usize,
    first_payload: bool,
    next_expected_count: Count,
}

impl Capture {
    pub fn new(port: u16) -> anyhow::Result<Self> {
        // Create UDP socket
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
        // Bind our listening address
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        socket.bind(&address.into())?;
        // Reuse local address without timeout
        socket.reuse_address()?;
        // Set the buffer size to 500MB (it will read as double, for some reason)
        let sock_buf_size = 256 * 1024 * 1024 * 2;
        socket.set_recv_buffer_size(sock_buf_size)?;
        // Check
        let current_buf_size = socket.recv_buffer_size()?;
        if current_buf_size != sock_buf_size * 2 {
            return Err(Error::SetRecvBufferFailed {
                expected: sock_buf_size * 2,
                found: current_buf_size,
            }
            .into());
        }
        // Replace the socket2 socket with a std socket
        let sock = socket.into();
        Ok(Self {
            sock,
            backlog: HashMap::with_capacity(BACKLOG_BUFFER_PAYLOADS * 2),
            drops: 0,
            processed: 0,
            first_payload: true,
            next_expected_count: 0,
        })
    }

    pub fn capture(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        let n = self.sock.recv(buf)?;
        if n != buf.len() {
            Err(Error::SizeMismatch(n).into())
        } else {
            Ok(())
        }
    }

    pub fn start(
        &mut self,
        payload_sender: Sender<ArrayVec<u8, PAYLOAD_SIZE>>,
        stats_send: Sender<Stats>,
        stats_polling_time: Duration,
    ) -> anyhow::Result<()> {
        let mut last_stats = Instant::now();
        let mut capture_buf = [0u8; PAYLOAD_SIZE];
        loop {
            // Capture into buf
            self.capture(&mut capture_buf[..])?;
            self.processed += 1;
            // Then, we get the count
            let this_count = count(&capture_buf[..]);
            // Send away the stats if the time has come (non blocking)
            if last_stats.elapsed() >= stats_polling_time {
                let _ = stats_send.try_send(Stats {
                    drops: self.drops,
                    processed: self.processed,
                });
                last_stats = Instant::now();
            }
            // Check first payload
            if self.first_payload {
                self.first_payload = false;
                self.next_expected_count = this_count + 1;
            } else if this_count == self.next_expected_count {
                self.next_expected_count += 1;
                // And send
                payload_sender.send(capture_buf.try_into().unwrap())?;
            } else if this_count < self.next_expected_count {
                // If the packet is from the past, we drop it
                self.drops += 1;
            } else if this_count > self.next_expected_count + BACKLOG_BUFFER_PAYLOADS as u64 {
                warn!(
                    "Futuristic payload, jumping forward. Gap size - {}",
                    this_count - self.next_expected_count
                );
                // The current payload is far enough in the future that we need to skip ahead
                // It would take too long to send out all of the backlog, so we empty it immediately
                self.drops += self.backlog.len();
                self.backlog.clear();
                payload_sender.send(capture_buf.try_into().unwrap())?;
                self.next_expected_count = this_count + 1;
            } else {
                // This packet is from the future, store it
                self.backlog.insert(this_count, capture_buf);
                // But before we do that, we could potentially drain stuff from the backlog
                while let Some(pl) = self.backlog.remove(&self.next_expected_count) {
                    payload_sender.send(pl.try_into().unwrap())?;
                    self.next_expected_count += 1;
                }
            }
        }
    }
}

/// Decode just the count from a byte array
fn count(pl: &[u8]) -> Count {
    u64::from_be_bytes(pl[0..8].try_into().unwrap())
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub drops: usize,
    pub processed: usize,
}

pub fn cap_task(
    port: u16,
    cap_send: Sender<ArrayVec<u8, PAYLOAD_SIZE>>,
    stats_send: Sender<Stats>,
) -> anyhow::Result<()> {
    info!("Starting capture task!");
    let mut cap = Capture::new(port).unwrap();
    cap.start(cap_send, stats_send, STATS_POLL_DURATION)
}

// This task will decode incoming packets and send to the ringbuffer and downsample tasks
pub fn decode_task(
    from_cap: Receiver<ArrayVec<u8, PAYLOAD_SIZE>>,
    to_split: Sender<Box<Payload>>,
) -> anyhow::Result<()> {
    info!("Starting decode");
    // Marker bool for packet 1 - everything following is ordered. We need this count number to work back out the actual time of the stream
    let mut first_packet = true;
    // Receive
    loop {
        // Get next writeable slot
        let mut slot = to_split.send_ref()?;
        let payload = from_cap
            .recv_ref()
            .ok_or_else(|| anyhow!("Channel closed"))?;
        // Decode into slot
        **slot = Payload::from_bytes(&payload);
        if first_packet {
            FIRST_PACKET.store(slot.count, Ordering::Relaxed);
            first_packet = false;
        }
    }
}

pub fn split_task(
    from_decode: Receiver<Box<Payload>>,
    to_downsample: Sender<Box<Payload>>,
    to_dumps: Sender<Box<Payload>>,
) -> anyhow::Result<()> {
    info!("Starting split");
    loop {
        // Get next slot for downsampling
        let mut slot = to_downsample.send_ref()?;
        **slot = *from_decode
            .recv()
            .ok_or_else(|| anyhow!("Channel closed"))?;
        // Attempt to send ref
        match to_dumps.try_send_ref() {
            Ok(mut dump_slot) => **dump_slot = **slot,
            Err(e) => match e {
                thingbuf::mpsc::errors::TrySendError::Full(_) => continue,
                thingbuf::mpsc::errors::TrySendError::Closed(_) => break,
                _ => todo!(),
            },
        }
    }
    Ok(())
}
