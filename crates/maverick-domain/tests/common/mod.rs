//! Shared helpers for the maverick-domain integration tests. Each test binary that
//! needs them declares `mod common;`. Not every binary uses every helper.
#![allow(dead_code)]

pub mod golden;

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake};

/// Runs a future to completion on the current thread. Enough for graders under test,
/// whose futures never wait on timers or sockets.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Arc::new(ThreadWaker(std::thread::current())).into();
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}
