use crate::common::{Stokes, BLOCK_TIMEOUT, CHANNELS};
use arrayvec::ArrayVec;
use faer::{col, Col, Mat};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use tokio::sync::broadcast;
use tracing::info;

// We're going to use faer's abstractions here because it's the highest performance rust
// linear algebra crate atm

const RFI_CLEANING_BLOCK_SIZE: usize = 16384;

/// Construct the vandermonde matrix for x values `xs`
fn vander(xs: &[f32], order: usize) -> Mat<f32> {
    Mat::from_fn(xs.len(), order + 1, |i, j| xs[i].powf(j as f32))
}

/// Use least-squares to solve the polynomial that best fits the data.
/// Returns the coefficents in order a+bx+cx^2... with vec![a,b,c...]
fn polyfit(xs: &[f32], ys: &[f32], order: usize) -> Col<f32> {
    vander(xs, order).svd().pseudoinverse() * col::from_slice::<f32>(ys)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_vander() {
        let xs = [1., 2., 3., 4.];
        let ys = [1., 4., 9., 16.];
        let coefs = polyfit(xs.as_slice(), ys.as_slice(), 2);
        dbg!(coefs);
        panic!();
    }
}

// /// Remove the RFI from the block we've accumulated
// fn clean_rfi(block: ArrayViewMut2<f32>) {}

// pub fn rfi_cleaning_task(
//     receiver: Receiver<Stokes>,
//     sender: Sender<ArrayVec<Stokes, RFI_CLEANING_BLOCK_SIZE>>,
//     mut shutdown: broadcast::Receiver<()>,
// ) -> eyre::Result<()> {
//     info!("Starting RFI cleaning task");
//     // Create the thread local block of data we're going to accumulate into and muck with
//     let mut block = Array::zeros((RFI_CLEANING_BLOCK_SIZE, CHANNELS));
//     let mut next_time_row = 0;

//     // Basic strategy here is to accumulate each stokes into a block, and once the block is full
//     // to do the "standard" RFI cleaning routines before passing the block to the exfil task.
//     // This would make it equivalent to being in between PSRDADA buffers, but you know, without PSRDADA,
//     // (which is good)
//     loop {
//         if shutdown.try_recv().is_ok() {
//             info!("RFI cleaning task task stopping");
//             break;
//         }
//         // Get the next stokes from the previous task
//         let stokes = match receiver.recv_timeout(BLOCK_TIMEOUT) {
//             Ok(p) => p,
//             Err(RecvTimeoutError::Timeout) => continue,
//             Err(RecvTimeoutError::Disconnected) => break,
//         };
//         // Create ndarray "ArrayView" from slice of stokes
//         let stokes_view = ArrayView::from_shape(CHANNELS, stokes.as_slice())?;
//         // Copy into the buffer
//         block.row_mut(next_time_row).assign(&stokes_view);
//         // Check if this was the last one, if so, perform the RFI cleaning and pass it along
//         todo!();
//         // Else increment the row ptr
//         next_time_row += 1;
//     }
//     Ok(())
// }
