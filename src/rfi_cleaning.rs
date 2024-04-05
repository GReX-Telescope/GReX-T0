use crate::common::{Stokes, BLOCK_TIMEOUT, CHANNELS};
use arrayvec::ArrayVec;
use faer::{
    col,
    prelude::*,
    reborrow::{Reborrow, ReborrowMut},
};
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

/// Computes the mean column
fn column_mean(data_view: MatRef<'_, f32>) -> Col<f32> {
    let n = data_view.ncols();
    let divisor = 1.0 / n as f32;
    data_view * Col::from_fn(n, |_| divisor)
}

/// Computes the mean row
fn row_mean(data_view: MatRef<'_, f32>) -> Row<f32> {
    let n = data_view.nrows();
    let divisor = 1.0 / n as f32;
    Row::from_fn(n, |_| divisor) * data_view
}

/// Detrend the frequency axis
pub fn detrend_freq_inplace(mut data_view: MatMut<'_, f32>, order: usize) {
    let n_freq = data_view.ncols();
    let n_time = data_view.nrows();
    // To detrend in frequency, we need to average across time (rows)
    let xs: Vec<_> = (0..n_freq).map(|x| x as f32).collect();
    let ys = row_mean(data_view.rb());
    // Create the vandermonde matrix for least squares fitting and poly eval
    let v = vander(&xs, order);
    // Then fit a polynomial to this data
    let coeffs = v.qr().solve_lstsq(ys.transpose());
    // Then use the vandermonde matrix to evaluate the polynomial
    let polyeval = v * coeffs;
    // Then subtract each column (frequency channel) by the result of the polynomial evaluation at that point
    // We need to iterate column-major for memory-order traversal
    for j in 0..data_view.ncols() {
        let mut col = data_view.rb_mut().col_mut(j);
        let weights = Col::from_fn(n_time, |_| polyeval[j]);
        col -= &weights;
    }
}

/// Detrend the time axis
pub fn detrend_time_inplace(mut data_view: MatMut<'_, f32>, order: usize) {
    let n_freq = data_view.ncols();
    let n_time = data_view.nrows();
    // To detrend in time, we need to average across freq (cols)
    let xs: Vec<_> = (0..n_time).map(|x| x as f32).collect();
    let ys = column_mean(data_view.rb());
    // Create the vandermonde matrix for least squares fitting and poly eval
    let v = vander(&xs, order);
    // Then fit a polynomial to this data
    let coeffs = v.qr().solve_lstsq(ys);
    // Then use the vandermonde matrix to evaluate the polynomial
    let polyeval = v * coeffs;
    // Construct the matrix
    // Then subtract each row (time) by the result of the polynomial evaluation at that point
    for j in 0..n_freq {
        let mut col = data_view.rb_mut().col_mut(j);
        col -= &polyeval;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use faer::mat;

    #[test]
    fn test_vander() {
        let xs = vander([1.0, 2.0, 3.0].as_slice(), 2);
        assert_eq!(xs.col_as_slice(0), [1.0, 1.0, 1.0].as_slice());
        assert_eq!(xs.col_as_slice(1), [1.0, 2.0, 3.0].as_slice());
        assert_eq!(xs.col_as_slice(2), [1.0, 4.0, 9.0].as_slice());
    }

    #[test]
    fn test_col_mean() {
        let a = mat![[1.0, 4.0], [2.0, 5.0], [3.0, 6.0]];
        let mu = column_mean(a.as_ref());
        assert_eq!(mu.as_slice(), [2.5f32, 3.5, 4.5].as_slice());
    }

    #[test]
    fn test_row_mean() {
        let a = mat![[1.0, 4.0], [2.0, 5.0], [3.0, 6.0]];
        let mu = row_mean(a.as_ref());
        assert_eq!(mu.as_slice(), [2.0f32, 5.0].as_slice());
    }

    #[test]
    fn test_detrend_freq() {
        let mut a = mat![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]];
        detrend_freq_inplace(a.as_mut(), 2);
        let detrended = mat![[-3.0f32, -3.0, -3.0], [0.0, 0.0, 0.0], [3.0, 3.0, 3.0]];
        for i in 0..3 {
            for j in 0..3 {
                assert!((a.get(i, j) - detrended.get(i, j)).abs() < 1e-5)
            }
        }
    }

    #[test]
    fn test_detrend_time() {
        let mut a = mat![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]];
        detrend_time_inplace(a.as_mut(), 2);
        let detrended = mat![[-1., 0., 1.], [-1.0, 0.0, 1.0], [-1.0, 0.0, 1.0f32]];
        for i in 0..3 {
            for j in 0..3 {
                assert!((a.get(i, j) - detrended.get(i, j)).abs() < 1e-5)
            }
        }
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
