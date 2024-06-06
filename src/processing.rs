//! Inter-thread processing (downsampling, etc)
use crate::common::{stokes_i, Payload, Stokes, BLOCK_TIMEOUT, CHANNELS};
use eyre::bail;
use thingbuf::mpsc::{
    blocking::{Sender, StaticReceiver, StaticSender},
    errors::RecvTimeoutError,
};
use tokio::sync::broadcast;
use tracing::info;

#[allow(clippy::missing_panics_doc)]
pub fn downsample_task(
    receiver: StaticReceiver<Payload>,
    sender: Sender<Stokes>,
    to_dumps: StaticSender<Payload>,
    downsample_power: u32,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    info!("Starting downsample task");
    let downsamp_iters = 2usize.pow(downsample_power);
    let mut downsamp_buf = [0f32; CHANNELS];
    let mut stokes_buf = [0f32; CHANNELS];
    let mut local_downsamp_iters = 0;

    loop {
        if shutdown.try_recv().is_ok() {
            info!("Downsample task stopping");
            break;
        }
        let payload = match receiver.recv_ref_timeout(BLOCK_TIMEOUT) {
            Ok(p) => p,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Closed) => break,
            Err(_) => unreachable!(),
        };
        // Send payload to dump (non-blocking)
        if let Err(thingbuf::mpsc::errors::TrySendError::Closed(_)) = to_dumps.try_send(*payload) {
            bail!("Channel closed");
        }
        // Compute Stokes I
        stokes_i(&mut stokes_buf, &payload);
        // Add to averaging bufs
        downsamp_buf
            .iter_mut()
            .zip(&stokes_buf)
            .for_each(|(x, y)| *x += y);

        // Increment the count
        local_downsamp_iters += 1;

        // Check for downsample exit condition
        if local_downsamp_iters == downsamp_iters {
            // Write averages directly into it
            downsamp_buf
                .iter_mut()
                .for_each(|v| *v /= local_downsamp_iters as f32);
            sender.send(downsamp_buf.into())?;

            // And reset averaging
            downsamp_buf.iter_mut().for_each(|v| *v = 0.0);
            local_downsamp_iters = 0;
        }
    }
    Ok(())
}
