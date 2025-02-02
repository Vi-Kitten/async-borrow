use std::{future::Future, ops::{Deref, DerefMut}, pin::Pin};

use futures::{stream::FusedStream, Stream, StreamExt};

// MARK: Spawner

/// Spawning handle for an Anchor.
///
/// *can be safely broadened as it can only spawn tasks.*
#[derive(Debug, Clone)]
pub struct Spawner</* In */ 'env, /* In */ T = ()>(
    futures::channel::mpsc::UnboundedSender</* In */ Pin<Box<dyn Future<Output = T> + 'env>>>,
);

impl<'env, T> Spawner<'env, T> {
    pub fn spawn(
        &mut self,
        future: impl Future<Output = T> + 'env,
    ) -> Result<(), futures::channel::mpsc::SendError> {
        self.0.start_send(Box::pin(future))
    }
}

// MARK: Anchor

/// An anchor to handle safe scoped task "spawning".
#[derive(Debug)]
pub struct Anchor</* Mix */ 'env, /* Mix */ T = ()> {
    receiver: futures::channel::mpsc::UnboundedReceiver<
        /* Out */ Pin<Box<dyn Future<Output = T> + 'env>>,
    >,
    pub spawner: Spawner</* In */ 'env, /* In */ T>,
}

impl<'env, T> Anchor<'env, T> {
    pub fn new() -> Self {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        Anchor {
            receiver,
            spawner: Spawner(sender),
        }
    }

    pub fn stream(self) -> Pool<'env, T> {
        Pool {
            receiver: self.receiver,
            tasks: futures::stream::FuturesUnordered::new(),
        }
    }
}

impl<'env, T> Deref for Anchor<'env, T> {
    type Target = Spawner<'env, T>;

    fn deref(&self) -> &Self::Target {
        &self.spawner
    }
}

impl<'env, T> DerefMut for Anchor<'env, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.spawner
    }
}

// MARK: Pool

/// A future for awaiting a collection of futures.
///
/// *can safely be narrowed as no more tasks can be spawned to it.*
#[derive(Debug)]
#[must_use = "streams do nothing unless polled"]
pub struct Pool</* Out */ 'env, /* Out */ T = ()> {
    receiver: futures::channel::mpsc::UnboundedReceiver<
        /* Out */ Pin<Box<dyn Future<Output = T> + 'env>>,
    >,
    tasks:
        futures::stream::FuturesUnordered</* Out */ Pin<Box<dyn Future<Output = T> + 'env>>>,
}

impl<'env, T> Stream for Pool<'env, T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        if self.receiver.is_terminated() {
            // only the tasks now
            return self.tasks.poll_next_unpin(cx);
        }
        loop {
            match self.receiver.poll_next_unpin(cx) {
                Poll::Ready(None) => {
                    if self.tasks.is_terminated() {
                        // end stream
                        return Poll::Ready(None);
                    } else {
                        break;
                    }
                }
                Poll::Ready(Some(task)) => {
                    self.tasks.push(task);
                    continue;
                }
                Poll::Pending => break,
            }
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        }
        // we have awaited all we can from the receiver
        if !self.tasks.is_terminated() {
            // more tasks to run
            match self.tasks.poll_next_unpin(cx) {
                Poll::Ready(Some(val)) => return Poll::Ready(Some(val)),
                Poll::Ready(None) => {
                    if self.receiver.is_terminated() {
                        // end stream
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending => (),
            }
        };
        // we have awaited all we can for the tasks
        Poll::Pending
    }
}

impl<'env, T> FusedStream for Pool<'env, T> {
    fn is_terminated(&self) -> bool {
        self.tasks.is_terminated() && self.receiver.is_terminated()
    }
}