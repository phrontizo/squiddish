use bytes::Bytes;
use hyper::body::{Body, Frame};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::Stream;

use crate::cache::DownloadChunk;

/// A streaming body that receives data from a broadcast channel.
/// Uses BroadcastStream internally for correct waker registration
/// (avoids busy-loop spinning with try_recv + wake_by_ref).
pub struct StreamingBody {
    stream: BroadcastStream<DownloadChunk>,
    initial_chunks: Vec<Bytes>,
    initial_index: usize,
    finished: bool,
}

impl StreamingBody {
    pub fn new(
        receiver: tokio::sync::broadcast::Receiver<DownloadChunk>,
        initial_chunks: Vec<Bytes>,
    ) -> Self {
        Self {
            stream: BroadcastStream::new(receiver),
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
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        if this.finished {
            return Poll::Ready(None);
        }

        // First, serve any initial chunks (for late joiners)
        if this.initial_index < this.initial_chunks.len() {
            let chunk = this.initial_chunks[this.initial_index].clone();
            this.initial_index += 1;
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }

        // Poll the BroadcastStream for new chunks
        match Pin::new(&mut this.stream).poll_next(cx) {
            Poll::Ready(Some(Ok(DownloadChunk::Data(data)))) => {
                Poll::Ready(Some(Ok(Frame::data(data))))
            }
            Poll::Ready(Some(Ok(DownloadChunk::Complete))) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(DownloadChunk::Error(e)))) => {
                this.finished = true;
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(n)))) => {
                tracing::warn!("Streaming body lagged by {} messages", n);
                this.finished = true;
                Poll::Ready(Some(Err(format!("Stream lagged by {} messages", n).into())))
            }
            Poll::Ready(None) => {
                // Channel closed
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
