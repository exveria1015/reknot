//! Owned task handles and Waker bridges shared by host and logic.
mod future;
mod waker;

pub use future::FfiFuture;
pub(crate) use waker::poll_result_future;
pub(crate) use waker::{host_waker_from, waker_from_host};
