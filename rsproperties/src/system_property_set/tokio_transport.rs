// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The property service exchange of `asynchronous::set`, on tokio.
//! [`prepare`] and [`finish`] are the sync `set`'s; only the socket I/O
//! differs:
//!
//! - **connect**: AF_UNIX reports a full listen backlog as `EAGAIN`, which
//!   tokio's `UnixStream::connect` returns as `WouldBlock` (it waits only
//!   for `EINPROGRESS`). Retried every 10 ms within [`SERVICE_IO_TIMEOUT`],
//!   as the sync connect does. Each attempt is a new socket: `socket`,
//!   `connect`, `close`.
//! - **send**: `send(MSG_NOSIGNAL)` through `try_io`, so a closed peer is
//!   `EPIPE` and never SIGPIPE; tokio's own write is the plain `write(2)`
//!   the sync path avoids. The whole frame within the same 2 s.
//! - **V2 reply**: no bound, on every target, so Android and Linux behave
//!   alike. The sync `set` waits at most 2 s outside Android; wrap this
//!   one in a timeout if needed.
//! - **V1 close wait**: `shutdown(Write)`, then reads until EOF for at most
//!   [`V1_CLOSE_WAIT`]; the outcome is not reported, as in the sync path.

use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::Interest;
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::{
    connect_timed_out, early_eof, finish, get_property_service_for_system_socket,
    get_property_service_socket, map_timeout_err, prepare, send_timed_out, write_zero, Reply,
    Route, SetRequest, SERVICE_IO_TIMEOUT, V1_CLOSE_WAIT,
};
use crate::errors::*;

/// Gap between connect attempts while the backlog is full.
const CONNECT_RETRY: Duration = Duration::from_millis(10);

/// `asynchronous::set`: checks and builds the request now, exchanges it
/// with the service when polled. Owns everything it needs, so the future
/// is `'static`.
pub(crate) fn set(name: &str, value: &str) -> impl Future<Output = Result<()>> + Send + 'static {
    let request = prepare(name, value);
    let name = name.to_owned();
    let value_len = value.len();
    async move {
        let request = request?;
        let sockets = Sockets::of(request.route);
        match exchange(&request, &sockets).await? {
            Some(status) => finish(&name, value_len, status),
            None => Ok(()),
        }
    }
}

/// The socket paths a request tries, in order.
struct Sockets {
    first: PathBuf,
    fallback: Option<PathBuf>,
}

impl Sockets {
    fn of(route: Route) -> Self {
        match route {
            Route::Default => Self {
                first: get_property_service_socket(),
                fallback: None,
            },
            Route::ForSystemFirst => Self {
                first: get_property_service_for_system_socket(),
                fallback: Some(get_property_service_socket()),
            },
        }
    }
}

/// Connect, send, then the V2 status (`Some`) or the V1 close wait.
async fn exchange(request: &SetRequest, sockets: &Sockets) -> Result<Option<i32>> {
    let stream = connect_first(sockets).await?;
    send(&stream, &request.frame).await?;
    match request.reply {
        Reply::V1Close => {
            close_wait(&stream).await;
            Ok(None)
        }
        Reply::V2Status => recv_status(&stream).await.map(Some),
    }
}

async fn connect_first(sockets: &Sockets) -> Result<UnixStream> {
    match (connect(&sockets.first).await, &sockets.fallback) {
        (Ok(stream), _) => Ok(stream),
        (Err(first_err), Some(fallback)) => {
            log::warn!(
                "Connect to {:?} failed ({first_err}); falling back to {fallback:?}",
                sockets.first
            );
            Ok(connect(fallback).await?)
        }
        (Err(e), None) => Err(Error::Io(e)),
    }
}

async fn connect(path: &Path) -> std::io::Result<UnixStream> {
    let attempts = async {
        loop {
            match UnixStream::connect(path).await {
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(CONNECT_RETRY).await;
                }
                other => return other,
            }
        }
    };
    timeout(SERVICE_IO_TIMEOUT, attempts)
        .await
        .unwrap_or_else(|_| Err(connect_timed_out(path, SERVICE_IO_TIMEOUT)))
}

async fn send(stream: &UnixStream, frame: &[u8]) -> Result<()> {
    let total = frame.len();
    let mut written = 0usize;
    let sending = async {
        while written < total {
            stream.writable().await?;
            match stream.try_io(Interest::WRITABLE, || {
                send_nosignal(stream, &frame[written..])
            }) {
                Ok(0) => return Err(write_zero()),
                Ok(n) => written += n,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(e) => return Err(map_timeout_err(e, "sending property service request")),
            }
        }
        Ok(())
    };
    match timeout(SERVICE_IO_TIMEOUT, sending).await {
        Ok(sent) => sent,
        Err(_) => Err(send_timed_out(written, total)),
    }
}

/// `send(MSG_NOSIGNAL)`. Elsewhere than Linux and Android a plain send:
/// macOS is a build target only, and its sockets need `SO_NOSIGPIPE`
/// instead, which tokio does not set (unlike the sync path's socket).
fn send_nosignal(stream: &UnixStream, buf: &[u8]) -> std::io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = rustix::net::SendFlags::NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = rustix::net::SendFlags::empty();
    rustix::net::send(stream, buf, flags).map_err(std::io::Error::from)
}

async fn recv_status(stream: &UnixStream) -> Result<i32> {
    let mut buf = [0u8; 4];
    let mut filled = 0usize;
    while filled < buf.len() {
        stream.readable().await?;
        match stream.try_read(&mut buf[filled..]) {
            Ok(0) => return Err(early_eof()),
            Ok(n) => filled += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
            Err(e) => return Err(map_timeout_err(e, "waiting for property service response")),
        }
    }
    Ok(i32::from_ne_bytes(buf))
}

/// The V1 ack: init closes the connection. Logged, never an error — the
/// frame went out in full, and bionic reports success however its own
/// 250 ms wait ends.
async fn close_wait(stream: &UnixStream) {
    let _ = rustix::net::shutdown(stream, rustix::net::Shutdown::Write);
    let drain = async {
        let mut buf = [0u8; 64];
        loop {
            if stream.readable().await.is_err() {
                return;
            }
            match stream.try_read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(e) => {
                    log::warn!("wait_for_socket_close: drain error ignored ({e})");
                    return;
                }
            }
        }
    };
    if timeout(V1_CLOSE_WAIT, drain).await.is_err() {
        log::warn!("wait_for_socket_close: timed out after {V1_CLOSE_WAIT:?}");
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::Instant;

    use zerocopy::IntoBytes;

    use super::super::{PropertyMessage, PROP_MSG_SETPROP, PROP_SUCCESS};
    use super::*;

    /// A socket path in a fresh directory, removed on drop.
    struct SocketDir(PathBuf);

    impl SocketDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rsp_aset_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for SocketDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn one(path: PathBuf) -> Sockets {
        Sockets {
            first: path,
            fallback: None,
        }
    }

    fn v2_request(name: &str, value: &str) -> SetRequest {
        SetRequest {
            frame: super::super::v2_frame(name, value).unwrap(),
            route: Route::Default,
            reply: Reply::V2Status,
        }
    }

    /// A server that accepts one connection, reads `frame_len` bytes,
    /// waits `delay`, writes `reply` and closes. Returns what it read.
    fn serve(
        listener: UnixListener,
        frame_len: usize,
        delay: Duration,
        reply: Vec<u8>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = vec![0u8; frame_len];
            stream.read_exact(&mut frame).unwrap();
            std::thread::sleep(delay);
            stream.write_all(&reply).unwrap();
            frame
        })
    }

    /// The service gets the frame `prepare` built — the sync `set`'s — and
    /// its status comes back.
    #[tokio::test]
    async fn v2_exchange_sends_the_prepared_frame() {
        let dir = SocketDir::new("v2");
        let path = dir.path("s");
        let request = v2_request("debug.rsp.async_set", "value");
        let server = serve(
            UnixListener::bind(&path).unwrap(),
            request.frame.len(),
            Duration::ZERO,
            PROP_SUCCESS.to_ne_bytes().to_vec(),
        );
        let status = exchange(&request, &one(path)).await.unwrap();
        assert_eq!(status, Some(PROP_SUCCESS));
        assert_eq!(server.join().unwrap(), request.frame);
    }

    /// V1: no reply; the wait lasts until the server's close, 25 ms after
    /// it read the frame, and ends well before 250 ms.
    #[tokio::test]
    async fn v1_exchange_ends_at_the_close() {
        let dir = SocketDir::new("v1");
        let path = dir.path("s");
        let message = PropertyMessage::new(PROP_MSG_SETPROP, "debug.rsp.v1", "1").unwrap();
        let request = SetRequest {
            frame: message.as_bytes().to_vec(),
            route: Route::Default,
            reply: Reply::V1Close,
        };
        let close_after = Duration::from_millis(25);
        let server = serve(
            UnixListener::bind(&path).unwrap(),
            request.frame.len(),
            close_after,
            Vec::new(),
        );
        let start = Instant::now();
        assert_eq!(exchange(&request, &one(path)).await.unwrap(), None);
        let elapsed = start.elapsed();
        assert!(elapsed >= close_after, "{elapsed:?}");
        assert!(elapsed < V1_CLOSE_WAIT, "{elapsed:?}");
        assert_eq!(server.join().unwrap(), request.frame);
    }

    /// The reply wait has no bound: a reply after 2.5 s — past the sync
    /// path's 2 s — is returned.
    #[tokio::test]
    async fn late_reply_is_returned() {
        let dir = SocketDir::new("late");
        let path = dir.path("s");
        let request = v2_request("debug.rsp.late", "1");
        let server = serve(
            UnixListener::bind(&path).unwrap(),
            request.frame.len(),
            Duration::from_millis(2500),
            PROP_SUCCESS.to_ne_bytes().to_vec(),
        );
        assert_eq!(
            exchange(&request, &one(path)).await.unwrap(),
            Some(PROP_SUCCESS)
        );
        server.join().unwrap();
    }

    /// A reply cut short is the sync path's `UnexpectedEof`.
    #[tokio::test]
    async fn short_reply_is_unexpected_eof() {
        let dir = SocketDir::new("short");
        let path = dir.path("s");
        let request = v2_request("debug.rsp.short", "1");
        let server = serve(
            UnixListener::bind(&path).unwrap(),
            request.frame.len(),
            Duration::ZERO,
            vec![0u8; 2],
        );
        let err = exchange(&request, &one(path)).await.unwrap_err();
        assert_eq!(err.to_string(), early_eof().to_string());
        server.join().unwrap();
    }

    /// A send to a peer that has closed is `EPIPE`, not SIGPIPE (libtest
    /// ignores SIGPIPE; the sync path's re-executed test covers the signal).
    #[tokio::test]
    async fn send_to_closed_peer_is_epipe() {
        let dir = SocketDir::new("epipe");
        let path = dir.path("s");
        let listener = UnixListener::bind(&path).unwrap();
        let (closed, wait_closed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            drop(listener.accept().unwrap());
            closed.send(()).unwrap();
        });
        let stream = connect(&path).await.unwrap();
        wait_closed.recv().unwrap();
        server.join().unwrap();
        let err = send(&stream, &[0u8; 64]).await.unwrap_err();
        assert!(
            matches!(&err, Error::Io(e) if e.kind() == ErrorKind::BrokenPipe),
            "{err}"
        );
    }

    /// A listener with backlog 0 that has one connection queued: further
    /// connects get `EAGAIN`.
    fn full_backlog(path: &Path) -> (rustix::fd::OwnedFd, std::os::unix::net::UnixStream) {
        use rustix::net as rnet;
        let fd = rnet::socket(rnet::AddressFamily::UNIX, rnet::SocketType::STREAM, None).unwrap();
        rnet::bind(&fd, &rnet::SocketAddrUnix::new(path).unwrap()).unwrap();
        rnet::listen(&fd, 0).unwrap();
        let queued = std::os::unix::net::UnixStream::connect(path).unwrap();
        (fd, queued)
    }

    /// Full backlog: the connect retries and succeeds once the listener
    /// accepts, and fails with the sync path's `TimedOut` when it never
    /// does.
    #[tokio::test]
    async fn connect_retries_a_full_backlog() {
        let dir = SocketDir::new("backlog");
        let path = dir.path("s");
        let (listener, _queued) = full_backlog(&path);
        let accepter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            // The queued connection, then ours.
            let _first = rustix::net::accept(&listener).unwrap();
            let _ours = rustix::net::accept(&listener).unwrap();
        });
        let start = Instant::now();
        connect(&path).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(250));
        accepter.join().unwrap();

        let stuck = dir.path("stuck");
        let (_listener, _queued) = full_backlog(&stuck);
        let start = Instant::now();
        let err = connect(&stuck).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert_eq!(
            err.to_string(),
            connect_timed_out(&stuck, SERVICE_IO_TIMEOUT).to_string()
        );
        assert!(start.elapsed() >= SERVICE_IO_TIMEOUT);
    }

    // Cancellation. The exchange awaits in the connect retries, in tokio's
    // write-readiness wait after `connect(2)` succeeded, and in the V2
    // reply or the V1 close wait. A drop in the readiness wait leaves a
    // connection with no byte sent, which the service answers with
    // `PROP_ERROR_READ_CMD`. A connect that completes is followed by the
    // send in the same poll — the socket is writable at once and a frame
    // this small goes out in one `send` — so a drop lands either before
    // any byte is sent or after the whole frame is. The two tests below
    // drop in the connect retries and in the V2 reply.

    /// Dropped while the connect retries a full backlog: no connection
    /// was made, so the service gets nothing.
    #[tokio::test]
    async fn dropped_in_the_connect_sends_nothing() {
        let dir = SocketDir::new("drop_connect");
        let path = dir.path("s");
        let (listener, _queued) = full_backlog(&path);
        let request = v2_request("debug.rsp.drop_connect", "1");
        let sockets = one(path);
        let exchanging = exchange(&request, &sockets);
        // Several 10 ms retries, well within the connect's 2 s.
        assert!(timeout(Duration::from_millis(100), exchanging)
            .await
            .is_err());

        // Only the queued connection is there: the dropped exchange left
        // no connection behind for the service to read a frame from.
        rustix::fs::fcntl_setfl(&listener, rustix::fs::OFlags::NONBLOCK).unwrap();
        let _first = rustix::net::accept(&listener).unwrap();
        assert_eq!(
            rustix::net::accept(&listener).unwrap_err(),
            rustix::io::Errno::AGAIN
        );
    }

    /// Dropped while it waits for the reply: the service has the whole
    /// frame, and its reply finds the connection closed.
    #[tokio::test]
    async fn dropped_in_the_reply_wait_sent_the_whole_frame() {
        let dir = SocketDir::new("drop_reply");
        let path = dir.path("s");
        let listener = UnixListener::bind(&path).unwrap();
        let request = v2_request("debug.rsp.drop_reply", "value");
        let frame_len = request.frame.len();
        let (got_frame, frame_in) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = vec![0u8; frame_len];
            stream.read_exact(&mut frame).unwrap();
            got_frame.send(()).unwrap();
            // The client's drop closes the connection: EOF, then the
            // reply fails (libtest ignores SIGPIPE).
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            let reply = stream.write_all(&PROP_SUCCESS.to_ne_bytes());
            (frame, rest, reply.map_err(|e| e.kind()))
        });

        let sockets = one(path);
        let mut exchanging = Box::pin(exchange(&request, &sockets));
        tokio::select! {
            done = &mut exchanging => panic!("returned without a reply: {done:?}"),
            got = frame_in => got.unwrap(),
        }
        drop(exchanging);

        let (frame, rest, reply) = server.join().unwrap();
        assert_eq!(frame, request.frame);
        assert!(rest.is_empty());
        assert_eq!(reply, Err(ErrorKind::BrokenPipe));
    }

    /// `sys.powerctl`'s route: a missing for_system socket falls back to
    /// the default one.
    #[tokio::test]
    async fn for_system_falls_back_to_default() {
        let dir = SocketDir::new("fallback");
        let path = dir.path("default");
        let request = v2_request("sys.powerctl", "reboot");
        let server = serve(
            UnixListener::bind(&path).unwrap(),
            request.frame.len(),
            Duration::ZERO,
            PROP_SUCCESS.to_ne_bytes().to_vec(),
        );
        let sockets = Sockets {
            first: dir.path("for_system"),
            fallback: Some(path),
        };
        assert_eq!(
            exchange(&request, &sockets).await.unwrap(),
            Some(PROP_SUCCESS)
        );
        server.join().unwrap();
    }

    /// A request the checks refuse fails at the `await` with the sync
    /// `set`'s error, without a connect.
    #[tokio::test]
    async fn refused_request_fails_like_sync() {
        for (name, value) in [("", "x"), ("bad..name", "x"), ("debug.rsp.nul", "a\0b")] {
            let sync = super::super::set(name, value).unwrap_err();
            let err = set(name, value).await.unwrap_err();
            assert_eq!(err.to_string(), sync.to_string(), "{name:?}");
        }
    }
}
