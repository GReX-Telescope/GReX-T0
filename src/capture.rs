//! Logic for capturing raw packets from the NIC, parsing them into payloads, and sending them to other processing threads

use crate::common::Payload;
use crossbeam_channel::Sender;
use log::{error, info, warn};
use socket2::{Domain, Socket, Type};
use std::net::UdpSocket;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::atomic::AtomicU64,
    time::{Duration, Instant},
};

/// Size of the packet count header
const TIMESTAMP_SIZE: usize = 8;
/// Total number of bytes in the spectra block of the UDP payload
const SPECTRA_SIZE: usize = 8192;
/// Total UDP payload size
pub const PAYLOAD_SIZE: usize = SPECTRA_SIZE + TIMESTAMP_SIZE;
/// Maximum number of payloads we want in the backlog
const BACKLOG_BUFFER_PAYLOADS: usize = 512;
/// Polling interval for stats
const STATS_POLL_DURATION: Duration = Duration::from_secs(10);
/// Global atomic to hold the count of the first packet
pub static FIRST_PACKET: AtomicU64 = AtomicU64::new(0);

#[derive(thiserror::Error, Debug)]
/// Errors that can be produced from captures
pub enum Error {
    #[error("We recieved a payload which wasn't the size we expected {0}")]
    SizeMismatch(usize),
    #[error("Failed to set the recv buffer size. We tried to set {expected}, but found {found}. Check sysctl net.core.rmem_max")]
    SetRecvBufferFailed { expected: usize, found: usize },
}

type Count = u64;

pub struct Capture {
    sock: UdpSocket,
    pub backlog: HashMap<Count, Payload>,
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
        // Set the buffer size to 1GB (it will read as double, for some reason)
        let sock_buf_size = 256 * 1024 * 1024 * 4;
        socket.set_recv_buffer_size(sock_buf_size)?;
        // Check
        let current_buf_size = socket.recv_buffer_size()?;
        // Two bytes off for some mysterious reason
        if current_buf_size != sock_buf_size * 2 - 2 {
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
        payload_sender: Sender<Box<Payload>>,
        stats_send: Sender<Stats>,
        stats_polling_time: Duration,
    ) -> anyhow::Result<()> {
        let mut last_stats = Instant::now();
        let mut capture_buf = [0u8; PAYLOAD_SIZE];
        loop {
            // Capture into buf
            self.capture(&mut capture_buf[..])?;
            // Transmute into a payload
            // Safety: We will always own the bytes, and the FPGA code ensures this is a valid thing to do
            // Also, we've checked that we've captured exactly 8200 bytes, which is the size of the payload
            let payload = unsafe { &*(capture_buf.as_ptr() as *const Payload) };
            self.processed += 1;
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
                self.next_expected_count = payload.count + 1;
                // And send the first one
                payload_sender.send(Box::new(*payload))?;
            } else if payload.count == self.next_expected_count {
                self.next_expected_count += 1;
                // And send
                payload_sender.send(Box::new(*payload))?;
            } else if payload.count < self.next_expected_count {
                // If the packet is from the past, we drop it
                warn!("Anachronistic payload, dropping packet");
                self.drops += 1;
            } else if payload.count > self.next_expected_count + BACKLOG_BUFFER_PAYLOADS as u64 {
                let gap = payload.count - self.next_expected_count;
                if gap < 2 * BACKLOG_BUFFER_PAYLOADS as u64 {
                    warn!(
                        "Futuristic payload, jumping forward. Gap size - {}",
                        payload.count - self.next_expected_count
                    );
                    // The current payload is far enough in the future that we need to skip ahead
                    // Before we do, though, drain the backlog, inserting dummy values for the missing chunks
                    for i in self.next_expected_count..payload.count {
                        match self.backlog.remove(&i) {
                            Some(p) => payload_sender.send(Box::new(p))?,
                            None => {
                                let dummy = Payload {
                                    count: i,
                                    ..Default::default()
                                };
                                payload_sender.send(Box::new(dummy))?;
                                self.drops += 1;
                            }
                        }
                    }
                } else {
                    // If the gap is so far ahead that it will take more than the cadence time (plus the RX buffer latency)
                    // to refill, something has gone horribly wrong and we need to reevaluate our purpose in life (and cause a resulting gap in the timestream)
                    error!("Distant futuristic payload, starting over. This results in a gap in the timestream and shouldn't happen");
                    self.drops += self.backlog.len();
                    self.backlog.clear();
                    payload_sender.send(Box::new(*payload))?;
                    self.next_expected_count = payload.count + 1;
                }
            } else {
                // This packet is from the future, but not so far that we think we're off pace,so store it
                self.backlog.insert(payload.count, *payload);
            }
            // Always try to catch up with the backlog
            while let Some(pl) = self.backlog.remove(&self.next_expected_count) {
                payload_sender.send(Box::new(pl))?;
                self.next_expected_count += 1;
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub drops: usize,
    pub processed: usize,
}

pub fn cap_task(
    port: u16,
    cap_send: Sender<Box<Payload>>,
    stats_send: Sender<Stats>,
) -> anyhow::Result<()> {
    info!("Starting capture task!");
    let mut cap = Capture::new(port).unwrap();
    cap.start(cap_send, stats_send, STATS_POLL_DURATION)
}
