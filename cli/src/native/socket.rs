//! The options of every socket that carries interactive traffic: a viewer's
//! pictures, sound and records, and the CDP commands, replies and events of
//! the driver, a Playwright program or a DevTools frontend. Nagle's
//! algorithm is off, so a write never waits for the peer to acknowledge the
//! one before it (Linux delays an acknowledgement up to 40 ms: a video unit
//! behind the toolbox waited 25 ms at the median, a CDP read behind a pending
//! command 27 ms; media-producer/toolbox and media-producer/cdp). Keepalive
//! probes a quiet connection, so a vanished peer is noticed.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::MaybeTlsStream;

/// Tunes one TCP socket. A socket that refuses an option still serves.
pub(crate) fn tune(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    // SockRef borrows the descriptor without taking ownership.
    let socket = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30));
    // TCP_KEEPINTVL, the time between probes once the first goes unanswered,
    // exists on most platforms but not on OpenBSD or Haiku.
    #[cfg(not(any(target_os = "openbsd", target_os = "haiku")))]
    let keepalive = keepalive.with_interval(Duration::from_secs(10));
    let _ = socket.set_tcp_keepalive(&keepalive);
}

/// Tunes the TCP socket under a dialed WebSocket, plain or TLS.
pub(crate) fn tune_dialed(stream: &MaybeTlsStream<TcpStream>) {
    match stream {
        MaybeTlsStream::Plain(stream) => tune(stream),
        MaybeTlsStream::Rustls(stream) => tune(stream.get_ref().0),
        _ => {}
    }
}

/// The next connection on `listener`, tuned.
pub(crate) async fn accept(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    let (stream, address) = listener.accept().await?;
    tune(&stream);
    Ok((stream, address))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both ends a connection can have: each write leaves at once and a
    /// quiet connection is probed.
    #[tokio::test]
    async fn an_accepted_and_a_dialed_socket_send_each_write_at_once_and_keep_alive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, dialed) = tokio::join!(accept(&listener), TcpStream::connect(address));
        let dialed = MaybeTlsStream::Plain(dialed.unwrap());
        tune_dialed(&dialed);
        let MaybeTlsStream::Plain(dialed) = &dialed else {
            unreachable!("a plain stream")
        };
        for stream in [&accepted.unwrap().0, dialed] {
            assert!(stream.nodelay().unwrap());
            assert!(socket2::SockRef::from(stream).keepalive().unwrap());
        }
    }
}
