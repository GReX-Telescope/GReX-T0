use crate::common::{Stokes, BLOCK_TIMEOUT};
use thingbuf::mpsc::{blocking::Receiver, errors::RecvTimeoutError};
use tokio::sync::broadcast;
use tracing::info;

/// A consumer that just grabs stokes off the channel and drops them
pub fn consumer(
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
