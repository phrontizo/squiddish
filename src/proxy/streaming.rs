use bytes::Bytes;
use hyper::body::{Body, Frame};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::broadcast;

use crate::cache::DownloadChunk;

/// A streaming body that receives data from a broadcast channel
pub struct StreamingBody {
    receiver: broadcast::Receiver<DownloadChunk>,
    initial_chunks: Vec<Bytes>,
    initial_index: usize,
    finished: bool,
}

impl StreamingBody {
    pub fn new(receiver: broadcast::Receiver<DownloadChunk>, initial_chunks: Vec<Bytes>) -> Self {
        Self {
            receiver,
            initial_chunks,
            initial_index: 0,
            finished: false,
        }
    }
}

impl Body for StreamingBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }

        // First, serve any initial chunks (for late joiners)
        if self.initial_index < self.initial_chunks.len() {
            let chunk = self.initial_chunks[self.initial_index].clone();
            self.initial_index += 1;
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }

        // Then receive from broadcast channel
        match self.receiver.try_recv() {
            Ok(DownloadChunk::Data(data)) => {
                Poll::Ready(Some(Ok(Frame::data(data))))
            }
            Ok(DownloadChunk::Complete) => {
                self.finished = true;
                Poll::Ready(None)
            }
            Ok(DownloadChunk::Error(e)) => {
                self.finished = true;
                Poll::Ready(Some(Err(e.into())))
            }
            Err(broadcast::error::TryRecvError::Empty) => {
                // No data available yet, register waker
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                // We fell behind, this is problematic for streaming
                tracing::warn!("Streaming body lagged by {} messages", n);
                self.finished = true;
                Poll::Ready(Some(Err(format!("Stream lagged by {} messages", n).into())))
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                self.finished = true;
                Poll::Ready(None)
            }
        }
    }
}
