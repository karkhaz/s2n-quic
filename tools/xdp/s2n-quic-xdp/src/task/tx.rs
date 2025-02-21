// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{if_xdp::RxTxDescriptor, ring, socket, syscall};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use s2n_quic_core::{
    slice::vectored_copy,
    sync::{spsc, worker},
};

/// Takes a queue of descriptors to be transmitted on a socket
pub async fn tx<N: Notifier>(outgoing: spsc::Receiver<RxTxDescriptor>, tx: ring::Tx, notifier: N) {
    Tx {
        outgoing,
        tx,
        notifier,
    }
    .await;
}

#[cfg(feature = "tokio")]
mod tokio_impl;

/// Notifies the implementor of progress on the TX ring
pub trait Notifier: Unpin {
    /// Notifies the subject that `count` items were transmitted on the TX ring
    fn notify(&mut self, tx: &mut ring::Tx, cx: &mut Context, count: u32);
    /// Notifies the subject that the TX ring doesn't have any capacity for transmission
    fn notify_empty(&mut self, tx: &mut ring::Tx, cx: &mut Context) -> Poll<()>;
}

impl Notifier for () {
    #[inline]
    fn notify(&mut self, _tx: &mut ring::Tx, _cx: &mut Context, _count: u32) {
        // nothing to do
    }

    #[inline]
    fn notify_empty(&mut self, _tx: &mut ring::Tx, _cx: &mut Context) -> Poll<()> {
        // nothing to do
        Poll::Ready(())
    }
}

impl<A: Notifier, B: Notifier> Notifier for (A, B) {
    #[inline]
    fn notify(&mut self, tx: &mut ring::Tx, cx: &mut Context, count: u32) {
        self.0.notify(tx, cx, count);
        self.1.notify(tx, cx, count);
    }

    #[inline]
    fn notify_empty(&mut self, tx: &mut ring::Tx, cx: &mut Context) -> Poll<()> {
        let a = self.0.notify_empty(tx, cx);
        let b = self.1.notify_empty(tx, cx);
        if a.is_ready() && b.is_ready() {
            a
        } else {
            Poll::Pending
        }
    }
}

impl Notifier for worker::Sender {
    #[inline]
    fn notify(&mut self, _tx: &mut ring::Tx, _cx: &mut Context, count: u32) {
        trace!("notifying worker to wake up with {count} entries");
        self.submit(count as _);
    }

    #[inline]
    fn notify_empty(&mut self, tx: &mut ring::Tx, _cx: &mut Context) -> Poll<()> {
        // there is no feedback mechanism for the worker::Sender so do nothing
        let _ = tx;
        Poll::Ready(())
    }
}

impl Notifier for socket::Fd {
    #[inline]
    fn notify(&mut self, tx: &mut ring::Tx, cx: &mut Context, _count: u32) {
        // notify the socket to ensure progress regardless of transmission count
        let _ = self.notify_empty(tx, cx);
    }

    #[inline]
    fn notify_empty(&mut self, tx: &mut ring::Tx, _cx: &mut Context) -> Poll<()> {
        // only notify the socket if it's set the needs wakeup flag
        if !tx.needs_wakeup() {
            trace!("TX ring doesn't need wake, returning early");
            return Poll::Ready(());
        }

        trace!("TX ring needs wakeup");
        let result = syscall::wake_tx(self);

        trace!("waking tx for progress {result:?}");

        Poll::Ready(())
    }
}

struct Tx<N: Notifier> {
    outgoing: spsc::Receiver<RxTxDescriptor>,
    tx: ring::Tx,
    notifier: N,
}

impl<N: Notifier> Future for Tx<N> {
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        let Self {
            outgoing,
            tx,
            notifier,
        } = self.get_mut();

        trace!("polling tx");

        for iteration in 0..10 {
            trace!("iteration {}", iteration);

            let count = match outgoing.poll_slice(cx) {
                Poll::Ready(Ok(slice)) => slice.len() as u32,
                Poll::Ready(Err(_)) => {
                    trace!("tx queue is closed; shutting down");
                    return Poll::Ready(());
                }
                Poll::Pending => {
                    trace!("tx queue out of items; sleeping");
                    return Poll::Pending;
                }
            };

            trace!("acquired {count} items from tx queues");

            let count = tx.acquire(count);

            trace!("acquired {count} items from TX ring");

            if count == 0 {
                // we couldn't acquire any items so notify the socket that we don't have capacity
                if notifier.notify_empty(tx, cx).is_ready() {
                    continue;
                } else {
                    return Poll::Pending;
                }
            }

            let mut outgoing = outgoing.slice();
            let (rx_head, rx_tail) = outgoing.peek();
            let (tx_head, tx_tail) = tx.data();

            let count = vectored_copy(&[rx_head, rx_tail], &mut [tx_head, tx_tail]);

            trace!("copied {count} items into TX ring");
            debug_assert_ne!(count, 0);

            tx.release(count as _);
            outgoing.release(count);
            notifier.notify(tx, cx, count as _);
        }

        // if we got here, we iterated 10 times and need to yield so we don't consume the event
        // loop too much
        trace!("waking self");
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        if_xdp::UmemDescriptor,
        task::testing::{random_delay, QUEUE_SIZE_LARGE, QUEUE_SIZE_SMALL, TEST_ITEMS},
    };
    use rand::prelude::*;
    use tokio::sync::oneshot;

    async fn execute_test(channel_size: usize) {
        let expected_total = TEST_ITEMS as u64;

        let (mut tx_send, tx_recv) = spsc::channel(channel_size);
        let (mut ring_rx, ring_tx) = ring::testing::rx_tx(channel_size as u32);
        let (worker_send, mut worker_recv) = worker::channel();
        let (done_send, done_recv) = oneshot::channel();

        tokio::spawn(tx(tx_recv, ring_tx, worker_send));

        tokio::spawn(async move {
            let mut addresses = (0..expected_total)
                .map(|address| UmemDescriptor { address }.with_len(0))
                .peekable();

            while addresses.peek().is_some() {
                if tx_send.acquire().await.is_err() {
                    return;
                }

                let batch_size = thread_rng().gen_range(1..channel_size);
                let mut slice = tx_send.slice();

                let _ = slice.extend(&mut (&mut addresses).take(batch_size));

                random_delay().await;
            }
        });

        tokio::spawn(async move {
            let mut total = 0;

            while let Some(credits) = worker_recv.acquire().await {
                let actual = ring_rx.acquire(1);

                if actual == 0 {
                    continue;
                }

                let (head, tail) = ring_rx.data();
                for entry in head.iter().chain(tail.iter()) {
                    assert_eq!(entry.address, total);
                    total += 1;
                }

                ring_rx.release(actual);
                worker_recv.finish(credits);
            }

            done_send.send(total).unwrap();
        });

        let actual_total = done_recv.await.unwrap();

        assert_eq!(expected_total, actual_total);
    }

    #[tokio::test]
    async fn tx_small_test() {
        execute_test(QUEUE_SIZE_SMALL).await;
    }

    #[tokio::test]
    async fn tx_large_test() {
        execute_test(QUEUE_SIZE_LARGE).await;
    }
}
