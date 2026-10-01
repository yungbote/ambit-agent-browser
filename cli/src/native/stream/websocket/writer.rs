//! One connection's measured wire debt, including library-written control frames.
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::stream::SplitSink;
use futures_util::SinkExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::WebSocketStream;

use super::super::video::LinkRate;

/// Count successful I/O, including automatic Pong/Close writes from the reader.
/// No rate, queue or timer lives at this boundary.
pub(super) struct Written<S> {
    pub inner: S,
    pub bytes: Arc<AtomicU64>,
}
impl<S: AsyncRead + Unpin> AsyncRead for Written<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Written<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(written)) = result {
            self.bytes.fetch_add(written as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub(super) type Socket = WebSocketStream<Written<TcpStream>>;

/// Serialization debt survives picture/stream replacement. Unknown bootstrap
/// retains its existing byte window; it does not invent a rate measurement.
pub(super) struct WireDebt {
    rate: Option<LinkRate>,
    debt_bits_per_second: Option<u32>,
    pub next_at: Instant,
}
impl WireDebt {
    pub fn new(now: Instant) -> Self {
        Self {
            rate: None,
            debt_bits_per_second: None,
            next_at: now,
        }
    }
    pub fn set_rate(&mut self, next: Option<LinkRate>, now: Instant) {
        if next == self.rate {
            return;
        }
        if let Some(next) = next {
            if let Some(previous) = self.debt_bits_per_second {
                let debt = self.next_at.saturating_duration_since(now).as_secs_f64()
                    * f64::from(previous)
                    / 8.0;
                self.next_at =
                    now + Duration::from_secs_f64(debt * 8.0 / f64::from(next.bits_per_second));
            }
            self.debt_bits_per_second = Some(next.bits_per_second);
        }
        self.rate = next;
    }
    pub fn charge(&mut self, bytes: u64, at: Instant) {
        if bytes == 0 {
            return;
        }
        if let Some(rate) = self.rate {
            self.next_at = at.max(self.next_at)
                + Duration::from_secs_f64(bytes as f64 * 8.0 / f64::from(rate.bits_per_second));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};

    fn rate(bits_per_second: u32) -> LinkRate {
        LinkRate {
            bits_per_second,
            burst_bytes: 65536,
        }
    }

    struct PartialIo {
        step: usize,
    }
    impl AsyncWrite for PartialIo {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.step += 1;
            match self.step {
                1 => Poll::Pending,
                2 => Poll::Ready(Ok(3)),
                _ => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed fixture",
                ))),
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn pending_and_error_writes_never_count_unwritten_bytes() {
        let bytes = Arc::new(AtomicU64::new(0));
        let mut io = Written {
            inner: PartialIo { step: 0 },
            bytes: bytes.clone(),
        };
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(Pin::new(&mut io).poll_write(&mut cx, &[0; 10]).is_pending());
        assert_eq!(bytes.load(Ordering::Acquire), 0);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &[0; 10]),
            Poll::Ready(Ok(3))
        ));
        assert_eq!(bytes.load(Ordering::Acquire), 3);
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &[0; 7]),
            Poll::Ready(Err(_))
        ));
        assert_eq!(bytes.load(Ordering::Acquire), 3);
    }

    #[test]
    fn all_wire_cost_accumulates_once_and_future_debt_survives_a_rate_epoch() {
        let now = Instant::now();
        let mut debt = WireDebt::new(now);
        debt.set_rate(Some(rate(500000)), now);
        debt.charge(1500, now);
        debt.charge(325, now);
        assert_eq!(
            debt.next_at,
            now + Duration::from_secs_f64(1825.0 / 62500.0)
        );
        let saved = debt.next_at;
        debt.charge(0, now + Duration::from_secs(1));
        assert_eq!(debt.next_at, saved);
        let half = now + Duration::from_secs_f64(1825.0 / 125000.0);
        debt.set_rate(None, half);
        assert_eq!(debt.next_at, saved);
        debt.set_rate(Some(rate(250000)), half);
        let remaining = debt.next_at.duration_since(half).as_secs_f64();
        assert!((remaining - 1825.0 / 62500.0).abs() < 1e-7);
    }

    #[tokio::test]
    async fn reader_written_pong_and_close_code_are_counted_once_after_upgrade() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let bytes = Arc::new(AtomicU64::new(0));
            let socket = tokio_tungstenite::accept_async(Written {
                inner: stream,
                bytes: bytes.clone(),
            })
            .await
            .unwrap();
            let (sink, mut reader) = socket.split();
            let mut writer = Writer::new(sink, bytes.clone());
            let handshake = writer.accounted;
            assert!(handshake > 0);
            writer.debt.set_rate(Some(rate(500000)), Instant::now());
            assert!(matches!(
                reader.next().await.unwrap().unwrap(),
                Message::Ping(_)
            ));
            // This read flushes the automatic Pong before the peer's text.
            assert_eq!(
                reader.next().await.unwrap().unwrap(),
                Message::Text("after".into())
            );
            assert_eq!(bytes.load(Ordering::Acquire) - handshake, 5);
            assert_eq!(
                writer.accounted, handshake,
                "no application send settled the Pong"
            );
            writer.send(Message::Text("reply".into())).await.unwrap();
            assert_eq!(writer.accounted - handshake, 12);
            let before = writer.debt.next_at;
            writer.settle(Instant::now());
            assert_eq!(
                writer.debt.next_at, before,
                "cumulative bytes were not charged twice"
            );
            writer
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "ok".into(),
                })))
                .await
                .unwrap();
            assert_eq!(
                writer.accounted - handshake,
                18,
                "close code consumes two real wire bytes"
            );
        });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/"))
            .await
            .unwrap();
        client.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::Pong(vec![1, 2, 3])
        );
        client.send(Message::Text("after".into())).await.unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap(),
            Message::Text("reply".into())
        );
        assert!(matches!(
            client.next().await.unwrap().unwrap(),
            Message::Close(_)
        ));
        server.await.unwrap();
    }
}

pub(super) struct Writer {
    sink: SplitSink<Socket, Message>,
    written: Arc<AtomicU64>,
    accounted: u64,
    pub debt: WireDebt,
}
impl Writer {
    /// Called after upgrade: HTTP handshake bytes never become media debt.
    pub fn new(sink: SplitSink<Socket, Message>, written: Arc<AtomicU64>) -> Self {
        let accounted = written.load(Ordering::Acquire);
        Self {
            sink,
            written,
            accounted,
            debt: WireDebt::new(Instant::now()),
        }
    }
    pub fn settle(&mut self, at: Instant) {
        let total = self.written.load(Ordering::Acquire);
        self.debt.charge(total - self.accounted, at);
        self.accounted = total;
    }
    pub async fn send(&mut self, message: Message) -> Result<(), Error> {
        let at = Instant::now();
        self.settle(at);
        let result = self.sink.send(message).await;
        // Charge from send start, not completion: blocking I/O already spent
        // part of this serialization time. Partial successful writes count.
        self.settle(at);
        result
    }
}
