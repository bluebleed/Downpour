//! Download queue — limits how many downloads run concurrently.
//!
//! TODO: wire this into `lib.rs` so `start_download` acquires a permit before
//! running and releases it on completion. Also persist queued items to disk so
//! they survive an app restart (pair with resume support in `downloader.rs`).
#![allow(dead_code)]

use tokio::sync::Semaphore;

pub struct Queue {
    permits: Semaphore,
}

impl Queue {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            permits: Semaphore::new(max_concurrent),
        }
    }
}
