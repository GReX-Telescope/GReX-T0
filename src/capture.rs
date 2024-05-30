//! Logic for capturing raw packets from the NIC, parsing them into payloads, and sending them to other processing threads

use crate::common::{Payload, FIRST_PACKET};
use socket2::{Domain, Socket, Type};
use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use thingbuf::mpsc::blocking::StaticSender;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

/// Size of the packet count header
const TIMESTAMP_SIZE: usize = 8;
/// Total number of bytes in the spectra block of the UDP payload
const SPECTRA_SIZE: usize = 8192;
/// Total UDP payload size
pub const PAYLOAD_SIZE: usize = SPECTRA_SIZE + TIMESTAMP_SIZE;
/// Polling interval for stats
const STATS_POLL_DURATION: Duration = Duration::from_secs(20);

#[derive(thiserror::Error, Debug)]
/// Errors that can be produced from captures
pub enum Error {
    #[error("We received a payload which wasn't the size we expected {0}")]
    SizeMismatch(usize),
    #[error("Failed to set the recv buffer size. We tried to set {expected}, but found {found}. Check sysctl net.core.rmem_max")]
    SetRecvBufferFailed { expected: usize, found: usize },
}

pub struct Capture {
    /// The socket itself
    sock: UdpSocket,
    /// How many packets we've dropped because the incoming one wasn't n+1
    pub drops: usize,
    /// How many packets from the past we've received (indicating there was a shuffle somewhere)
    pub shuffled: usize,
    /// The number of packets we've actually processed
    pub processed: usize,
    /// Marker bool for the first packet
    first_payload: bool,
    /// The next payload count we expect
    next_expected_count: u64,
}

impl Capture {
    pub fn new(port: u16) -> eyre::Result<Self> {
        // Create UDP socket
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
        // Bind our listening address
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        socket.bind(&address.into())?;
        // Reuse local address without timeout
        socket.reuse_address()?;
        // Set the buffer size to 256MiB (it will read as double, for some reason)
        let sock_buf_size = 256 * 1024 * 1024;
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
        // Set into nonblocking mode
        socket.set_nonblocking(true);
        // Replace the socket2 socket with a std socket
        let sock = socket.into();
        Ok(Self {
            sock,
            drops: 0,
            processed: 0,
            shuffled: 0,
            first_payload: true,
            next_expected_count: 0,
        })
    }

    pub fn capture(&mut self, buf: &mut [u8]) -> eyre::Result<()> {
        loop {
            match self.sock.recv(buf) {
                Ok(n) => {
                    if n != buf.len() {
                        return Err(Error::SizeMismatch(n).into());
                    } else {
                        return Ok(());
                    }
                }
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn start(
        &mut self,
        payload_sender: StaticSender<Payload>,
        stats_send: SyncSender<Stats>,
        stats_polling_time: Duration,
        mut shutdown: broadcast::Receiver<()>,
    ) -> eyre::Result<()> {
        let mut last_stats = Instant::now();
        let mut capture_buf = [0u8; PAYLOAD_SIZE];
        loop {
            // Look for shutdown signal
            if shutdown.try_recv().is_ok() {
                info!("Capture task stopping");
                break;
            }
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
                    shuffled: self.shuffled,
                });
                last_stats = Instant::now();
            }
            // Check first payload
            if self.first_payload {
                self.first_payload = false;
                // And send the first one
                payload_sender.send(*payload)?;
                FIRST_PACKET.swap(payload.count, Ordering::Acquire);
                self.next_expected_count = payload.count + 1;
            } else if payload.count == self.next_expected_count {
                self.next_expected_count += 1;
                // And send
                payload_sender.send(*payload)?;
            } else if payload.count < self.next_expected_count {
                // If the packet is from the past, we drop it
                warn!("Anachronistic payload, dropping packet");
                self.shuffled += 1;
            } else {
                // payload.count > self.next_expected_count
                // Packets were dropped, fill in with zeros (hopefully not too many)
                let drops = payload.count - self.next_expected_count;
                warn!("Jump in packet count, dropping {} packets", drops);
                for d in 0..drops {
                    // Create the payload in it's place
                    let pl = Payload {
                        count: self.next_expected_count + d,
                        ..Default::default()
                    };
                    // And send
                    payload_sender.send(pl)?;
                }
                // Don't forget to send *this* payload!!
                payload_sender.send(*payload)?;
                // Increment our drops counter
                self.drops += drops as usize;
                // And finally update the next expected
                self.next_expected_count = payload.count + 1;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
/// Statistics we send to the monitoring thread
pub struct Stats {
    pub drops: usize,
    pub processed: usize,
    pub shuffled: usize,
}

pub fn cap_task(
    port: u16,
    cap_send: StaticSender<Payload>,
    stats_send: SyncSender<Stats>,
    shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting capture task!");
    let mut cap = Capture::new(port).unwrap();
    cap.start(cap_send, stats_send, STATS_POLL_DURATION, shutdown)
}
