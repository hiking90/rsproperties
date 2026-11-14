// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The reply to a `ctl.*` request: [`ControlReply`], which the control
//! handler gets with the message and which the host answers once it has
//! handled the message.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use log::{debug, info, warn};
use rustix::io::Errno;
use rustix::net::SendFlags;

use rsproperties::wire::{PROP_ERROR_HANDLE_CONTROL_MESSAGE, PROP_SUCCESS};

use crate::socket_service::HandlerCallsStop;

/// The connection of a `ctl.*` request, taken out of the tokio reactor so
/// that the reply can be sent from any thread, with or without a runtime.
/// init's `SendControlMessage` releases the socket fd to the queued
/// message in the same way, and `HandleControlMessages` sends the reply on
/// it and closes it.
pub(crate) struct ReplyFd(OwnedFd);

impl ReplyFd {
    /// Fails only when the reactor cannot deregister the stream; the
    /// connection is closed then.
    pub(crate) fn new(stream: tokio::net::UnixStream) -> std::io::Result<Self> {
        // `into_std` leaves the fd in non-blocking mode.
        Ok(Self(stream.into_std()?.into()))
    }

    /// Sends `code` and closes the connection.
    ///
    /// The send never waits (`MSG_DONTWAIT`). These four bytes are the
    /// first the service writes to the connection, so the send buffer has
    /// room for them and `EAGAIN` is not expected; it is returned like any
    /// other error. `MSG_NOSIGNAL` turns a peer that already closed into
    /// `EPIPE` instead of `SIGPIPE`, as `SocketService::send_response`
    /// does for the replies sent from the connection task.
    pub(crate) fn send(self, code: i32) -> std::io::Result<()> {
        let bytes = code.to_ne_bytes();
        let mut written = 0;
        while written < bytes.len() {
            match rustix::net::send(
                &self.0,
                &bytes[written..],
                SendFlags::NOSIGNAL | SendFlags::DONTWAIT,
            ) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => written += n,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// What the [`ControlReply`]s of one service share with it. It holds no
/// handler and no queue, so a reply that the host keeps does not keep any
/// other part of the service alive.
pub(crate) struct ReplyState {
    stop: HandlerCallsStop,
    /// The replies made by [`ControlReply::to_client`] that have not been
    /// answered or dropped.
    unanswered: AtomicUsize,
}

impl ReplyState {
    pub(crate) fn new(stop: HandlerCallsStop) -> Self {
        Self {
            stop,
            unanswered: AtomicUsize::new(0),
        }
    }

    /// The number of `ctl.*` requests that have a [`ControlReply`] and no
    /// reply yet: those queued for the handler worker and those the host
    /// holds.
    pub(crate) fn unanswered(&self) -> usize {
        self.unanswered.load(Ordering::SeqCst)
    }

    /// The reply to a `ctl.<action>` request that gets no answer from the
    /// host: init's reply for `accept_messages` false once the handler
    /// calls are stopped (`PROP_SUCCESS` for `ctl.stop`), and a failure
    /// otherwise. init compares the whole action with `stop`.
    pub(crate) fn code_without_answer(&self, action: &str) -> i32 {
        if self.stop.is_stopped() && action == "stop" {
            PROP_SUCCESS
        } else {
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        }
    }
}

/// The reply to one `ctl.*` request. A
/// [control handler](crate::ServiceOptions::control_handler) gets it with
/// the [`ControlMessage`](crate::ControlMessage), and the host answers
/// with [`send`](Self::send): in the handler call or after it, from any
/// thread. init queues the client's socket with the message in the same
/// way and answers on it once its main thread has handled the message
/// (`QueueControlMessage`, `HandleControlMessages`).
///
/// ```
/// use rsproperties_service::{ControlMessage, ControlReply};
///
/// let handler = |msg: ControlMessage, reply: ControlReply| reply.send(msg.action == "start");
///
/// let (reply, probe) = ControlReply::for_test();
/// handler(ControlMessage::new("start", "my_service", None), reply);
/// assert_eq!(probe.blocking_answer(), Some(true));
/// ```
///
/// The client waits until the reply has been answered or dropped. bionic's
/// `__system_property_set` and the `rsproperties` client on Android wait
/// without a timeout, as they do for init. The `rsproperties` client on
/// other targets gives up after 2 s; an answer sent later is discarded.
///
/// # Dropping a reply
///
/// A reply that is dropped without [`send`](Self::send) answers
/// `PROP_ERROR_HANDLE_CONTROL_MESSAGE`, so a request the host lost (a
/// panic, a closed channel) does not keep its client waiting. Once the
/// handler calls are stopped
/// ([`ServiceHandle::stop_handler_calls`](crate::ServiceHandle::stop_handler_calls),
/// or a powerctl handler that returned
/// [`PowerctlOutcome::ShuttingDown`](crate::PowerctlOutcome::ShuttingDown)),
/// a dropped reply to `ctl.stop` answers `PROP_SUCCESS` instead: the reply
/// a `ctl.stop` request read after the stop gets. A host that is going
/// down can therefore drop the replies it still holds. `send` always
/// answers what it was given.
///
/// # What a reply holds
///
/// The client's connection (one fd) and nothing else of the service. It
/// can be answered after [`ServiceHandle::shutdown`](crate::ServiceHandle::shutdown)
/// has returned and after the tokio runtime has been dropped; neither
/// waits for it. While 102 requests are unanswered the service refuses
/// further `ctl.*` requests (see
/// [`ServiceOptions::control_handler`](crate::ServiceOptions::control_handler)).
#[must_use = "a ControlReply that is dropped answers its request as failed"]
pub struct ControlReply {
    /// `None` once answered.
    to: Option<ReplyTo>,
}

enum ReplyTo {
    Client {
        fd: ReplyFd,
        /// The message's action, which decides the reply of a drop.
        action: String,
        state: Arc<ReplyState>,
    },
    Probe(tokio::sync::oneshot::Sender<bool>),
}

impl ControlReply {
    /// The reply to the `ctl.<action>` request that came on `fd`. Counts
    /// as unanswered in `state` until it is answered or dropped.
    pub(crate) fn to_client(fd: ReplyFd, action: String, state: Arc<ReplyState>) -> Self {
        state.unanswered.fetch_add(1, Ordering::SeqCst);
        Self {
            to: Some(ReplyTo::Client { fd, action, state }),
        }
    }

    /// A reply that is not connected to a client, to test a handler or the
    /// code a handler passes its replies to. The [`ControlReplyProbe`]
    /// tells what became of it.
    ///
    /// ```
    /// use rsproperties_service::ControlReply;
    ///
    /// let (reply, probe) = ControlReply::for_test();
    /// std::thread::spawn(move || reply.send(false));
    /// assert_eq!(probe.blocking_answer(), Some(false));
    ///
    /// let (reply, probe) = ControlReply::for_test();
    /// drop(reply);
    /// assert_eq!(probe.blocking_answer(), None);
    /// ```
    pub fn for_test() -> (Self, ControlReplyProbe) {
        let (answer, probe) = tokio::sync::oneshot::channel();
        (
            Self {
                to: Some(ReplyTo::Probe(answer)),
            },
            ControlReplyProbe(probe),
        )
    }

    /// Answers the request and closes the client's connection: `true` is
    /// `PROP_SUCCESS`, `false` is `PROP_ERROR_HANDLE_CONTROL_MESSAGE`,
    /// the two codes init's `HandleControlMessages` sends.
    ///
    /// Does not wait and needs no tokio runtime. A client that has already
    /// closed its connection is not an error: the answer is discarded, as
    /// init ignores the result of its `send`.
    pub fn send(mut self, handled: bool) {
        self.finish(Some(handled));
    }

    /// Answers with `handled`, or with the reply of a drop for `None`.
    /// Does nothing the second time.
    fn finish(&mut self, handled: Option<bool>) {
        match self.to.take() {
            None => {}
            Some(ReplyTo::Probe(answer)) => {
                if let Some(handled) = handled {
                    // The probe may have been dropped.
                    let _ = answer.send(handled);
                }
            }
            Some(ReplyTo::Client { fd, action, state }) => {
                let code = match handled {
                    Some(true) => {
                        info!("Control message ctl.{action} handled");
                        PROP_SUCCESS
                    }
                    Some(false) => {
                        warn!("Control message ctl.{action} failed");
                        PROP_ERROR_HANDLE_CONTROL_MESSAGE
                    }
                    None => {
                        let code = state.code_without_answer(&action);
                        info!("Control message ctl.{action} was not answered; replying {code:#x}");
                        code
                    }
                };
                if let Err(e) = fd.send(code) {
                    debug!("The reply to ctl.{action} was not sent: {e}");
                }
                // After the send, which closed the fd: the count bounds
                // the fds that unanswered requests hold.
                state.unanswered.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

impl Drop for ControlReply {
    fn drop(&mut self) {
        self.finish(None);
    }
}

impl std::fmt::Debug for ControlReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ControlReply");
        if let Some(ReplyTo::Client { action, .. }) = &self.to {
            debug.field("action", action);
        }
        debug.finish_non_exhaustive()
    }
}

/// Tells what became of the reply made with it by
/// [`ControlReply::for_test`].
#[derive(Debug)]
pub struct ControlReplyProbe(tokio::sync::oneshot::Receiver<bool>);

impl ControlReplyProbe {
    /// Waits until the reply has been answered or dropped: `Some` with the
    /// value given to [`ControlReply::send`], or `None` for a reply that
    /// was dropped without an answer (to which the service would answer
    /// as described under "Dropping a reply" on [`ControlReply`]).
    pub async fn answer(self) -> Option<bool> {
        self.0.await.ok()
    }

    /// [`answer`](Self::answer) for code that is not async.
    ///
    /// # Panics
    ///
    /// When called from a task of a tokio runtime, where it would block
    /// the runtime's thread.
    pub fn blocking_answer(self) -> Option<bool> {
        self.0.blocking_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use crate::socket_service::tests::{in_child, run_in_child};

    /// A client's end with a read timeout, and the service's end as the
    /// connection task hands it over.
    fn connection() -> (UnixStream, ReplyFd) {
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        server.set_nonblocking(true).unwrap();
        let server = tokio::net::UnixStream::from_std(server).unwrap();
        (client, ReplyFd::new(server).unwrap())
    }

    /// The reply code, then the end of the stream: the reply closes the
    /// connection.
    fn read_code(client: &mut UnixStream) -> i32 {
        let mut code = [0u8; 4];
        client.read_exact(&mut code).unwrap();
        assert_eq!(client.read(&mut [0u8; 1]).unwrap(), 0, "must be closed");
        i32::from_ne_bytes(code)
    }

    fn state() -> (Arc<ReplyState>, HandlerCallsStop) {
        let stop = HandlerCallsStop::default();
        (Arc::new(ReplyState::new(stop.clone())), stop)
    }

    #[tokio::test]
    async fn send_answers_and_closes_and_uncounts() {
        let (state, _stop) = state();
        for (handled, code) in [
            (true, PROP_SUCCESS),
            (false, PROP_ERROR_HANDLE_CONTROL_MESSAGE),
        ] {
            let (mut client, fd) = connection();
            let reply = ControlReply::to_client(fd, "start".to_owned(), state.clone());
            assert_eq!(state.unanswered(), 1);
            reply.send(handled);
            assert_eq!(state.unanswered(), 0);
            assert_eq!(read_code(&mut client), code);
        }
    }

    /// A dropped reply is a failure, and for `ctl.stop` a success once the
    /// handler calls are stopped. The flag is read at the drop. `send`
    /// answers what it was given, also after the stop.
    #[tokio::test]
    async fn dropped_reply_answers_by_the_stop_flag() {
        let (state, stop) = state();
        let reply_to = |action: &str| {
            let (client, fd) = connection();
            (
                client,
                ControlReply::to_client(fd, action.to_owned(), state.clone()),
            )
        };

        let (mut stop_client, stop_reply) = reply_to("stop");
        drop(stop_reply);
        assert_eq!(
            read_code(&mut stop_client),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );

        let made_before_the_stop = [
            reply_to("stop"),
            reply_to("start"),
            reply_to("interface_stop"),
        ];
        let (mut refused_client, refused) = reply_to("stop");
        let (mut handled_client, handled) = reply_to("start");
        assert_eq!(state.unanswered(), 5);
        stop.stop();
        for ((mut client, reply), code) in made_before_the_stop.into_iter().zip([
            PROP_SUCCESS,
            PROP_ERROR_HANDLE_CONTROL_MESSAGE,
            PROP_ERROR_HANDLE_CONTROL_MESSAGE,
        ]) {
            drop(reply);
            assert_eq!(read_code(&mut client), code);
        }
        refused.send(false);
        assert_eq!(
            read_code(&mut refused_client),
            PROP_ERROR_HANDLE_CONTROL_MESSAGE
        );
        handled.send(true);
        assert_eq!(read_code(&mut handled_client), PROP_SUCCESS);
        assert_eq!(state.unanswered(), 0);
    }

    /// A reply made in a runtime is answered after that runtime is gone,
    /// from a thread that has none.
    #[test]
    fn send_needs_no_runtime() {
        let (state, _stop) = state();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (mut client, reply) = runtime.block_on(async {
            let (client, fd) = connection();
            (
                client,
                ControlReply::to_client(fd, "start".to_owned(), state.clone()),
            )
        });
        drop(runtime);
        std::thread::spawn(move || reply.send(true)).join().unwrap();
        assert_eq!(read_code(&mut client), PROP_SUCCESS);
        assert_eq!(state.unanswered(), 0);
    }

    #[tokio::test]
    async fn probe_reports_the_answer_or_the_drop() {
        let (reply, probe) = ControlReply::for_test();
        reply.send(true);
        assert_eq!(probe.answer().await, Some(true));

        let (reply, probe) = ControlReply::for_test();
        let waiting = tokio::spawn(probe.answer());
        drop(reply);
        assert_eq!(waiting.await.unwrap(), None);

        // A dropped probe does not affect the reply.
        let (reply, probe) = ControlReply::for_test();
        drop(probe);
        reply.send(false);
    }

    /// An answer to a client that already closed is discarded: no
    /// `SIGPIPE` even with the signal at `SIG_DFL` (C/JNI hosts), and the
    /// request no longer counts as unanswered. libtest ignores `SIGPIPE`,
    /// so the check runs in a child that restores the default.
    #[test]
    fn answer_to_closed_client_is_discarded_without_sigpipe() {
        if !in_child() {
            run_in_child(
                "control_reply::tests::answer_to_closed_client_is_discarded_without_sigpipe",
            );
            return;
        }
        // SAFETY: `libc::signal` is the libc crate's declaration of C
        // `signal` for this target. `SIG_DFL` installs no handler, so no
        // Rust code runs in signal context. The disposition is
        // process-wide; this child process runs only this test (`--exact`).
        let prev = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        assert_ne!(prev, libc::SIG_ERR, "signal(SIGPIPE, SIG_DFL) failed");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (state, _stop) = state();
        let (client, fd) = runtime.block_on(async { connection() });
        drop(client);
        let reply = ControlReply::to_client(fd, "start".to_owned(), state.clone());
        reply.send(true);
        assert_eq!(state.unanswered(), 0);

        let (client, fd) = runtime.block_on(async { connection() });
        drop(client);
        let err = fd.send(PROP_SUCCESS).expect_err("the peer has closed");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
