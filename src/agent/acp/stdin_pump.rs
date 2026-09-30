//! Read stdin from the very start and report when it closes.
//!
//! `rust-bot acp` may have to wait for the workspace lock *before* it serves
//! the connection, and building the runtime writes to the workspace. If the
//! client closes stdin during that wait, the process must exit without touching
//! anything, so something has to be reading stdin already. A background thread
//! reads it; the bytes are buffered for the protocol connection (nothing the
//! client sent is lost) and an [`EofSignal`] fires when the client is gone.

use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::io::AsyncRead;
use tokio::sync::{mpsc, watch};

/// Bytes read from the source per chunk.
const CHUNK_SIZE: usize = 8 * 1024;

/// The buffered input plus the end-of-input signal.
pub struct StdinPump {
    pub reader: PumpReader,
    pub eof: EofSignal,
}

/// The buffered bytes as an async reader for the protocol transport.
pub struct PumpReader {
    chunks: mpsc::UnboundedReceiver<Vec<u8>>,
    current: Vec<u8>,
    position: usize,
}

/// Completes once the source has reached end of input.
pub struct EofSignal(watch::Receiver<bool>);

impl EofSignal {
    /// Wait until the client has closed its end.
    pub async fn reached(&mut self) {
        // A dropped sender means the reading thread is gone, which is also "closed".
        let _ = self.0.wait_for(|reached| *reached).await;
    }
}

impl AsyncRead for PumpReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            if this.position < this.current.len() {
                let available = &this.current[this.position..];
                let count = available.len().min(buf.len());
                buf[..count].copy_from_slice(&available[..count]);
                this.position += count;
                return Poll::Ready(Ok(count));
            }
            match this.chunks.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    this.current = chunk;
                    this.position = 0;
                }
                // The reading thread ended and everything it read was consumed.
                Poll::Ready(None) => return Poll::Ready(Ok(0)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Start reading `source` on a background thread.
pub fn spawn_pump<R: Read + Send + 'static>(mut source: R) -> StdinPump {
    let (chunk_tx, chunk_rx) = mpsc::unbounded_channel();
    let (eof_tx, eof_rx) = watch::channel(false);

    std::thread::spawn(move || {
        let mut buffer = vec![0u8; CHUNK_SIZE];
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if chunk_tx.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Tell waiters first, then drop the sender so the reader sees end of input.
        let _ = eof_tx.send(true);
    });

    StdinPump {
        reader: PumpReader {
            chunks: chunk_rx,
            current: Vec::new(),
            position: 0,
        },
        eof: EofSignal(eof_rx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::AsyncReadExt;
    use std::io::Cursor;
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    /// A `Read` that blocks until the test sends bytes or closes the channel.
    struct ChannelSource(std_mpsc::Receiver<Vec<u8>>);

    impl Read for ChannelSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.recv() {
                Ok(bytes) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Err(_) => Ok(0),
            }
        }
    }

    #[tokio::test]
    async fn everything_read_is_delivered_in_order_then_eof() {
        let mut pump = spawn_pump(Cursor::new(b"line one\nline two\n".to_vec()));
        let mut text = String::new();
        pump.reader.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "line one\nline two\n");
    }

    #[tokio::test]
    async fn large_input_is_split_across_reads_without_loss() {
        let data: Vec<u8> = (0..(CHUNK_SIZE * 3 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut pump = spawn_pump(Cursor::new(data.clone()));
        let mut received = Vec::new();
        // Read with a small buffer to force several partial reads per chunk.
        let mut buffer = [0u8; 1000];
        loop {
            let count = pump.reader.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(received, data);
    }

    #[tokio::test]
    async fn eof_fires_when_the_source_ends() {
        let mut pump = spawn_pump(Cursor::new(Vec::new()));
        tokio::time::timeout(Duration::from_secs(5), pump.eof.reached())
            .await
            .expect("eof must be reported");
    }

    #[tokio::test]
    async fn eof_does_not_fire_while_the_source_is_open_and_does_after_it_closes() {
        let (sender, receiver) = std_mpsc::channel();
        let mut pump = spawn_pump(ChannelSource(receiver));

        sender.send(b"hello".to_vec()).unwrap();
        let waited = tokio::time::timeout(Duration::from_millis(200), pump.eof.reached()).await;
        assert!(
            waited.is_err(),
            "no EOF while the client is still connected"
        );

        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), pump.eof.reached())
            .await
            .expect("eof after the client closes");
    }

    #[tokio::test]
    async fn bytes_sent_before_eof_are_still_readable_after_it() {
        let (sender, receiver) = std_mpsc::channel();
        let mut pump = spawn_pump(ChannelSource(receiver));
        sender.send(b"initialize-request\n".to_vec()).unwrap();
        drop(sender);
        pump.eof.reached().await;

        let mut text = String::new();
        pump.reader.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "initialize-request\n");
    }

    #[tokio::test]
    async fn an_empty_read_buffer_is_not_mistaken_for_eof() {
        let mut pump = spawn_pump(Cursor::new(b"x".to_vec()));
        assert_eq!(pump.reader.read(&mut []).await.unwrap(), 0);
        let mut text = String::new();
        pump.reader.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "x");
    }
}
