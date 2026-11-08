//! The loopback listener a browser sign-in waits on (RFC 8252 section 7.3).
//!
//! A program that signs in through the browser binds `127.0.0.1` on a port
//! the system picks, sends the browser to the authorization page with
//! `http://127.0.0.1:<port>/callback` as its redirect, and waits here for the
//! browser to come back. Only `GET /callback` is served; anything else is a
//! 404. A callback whose `state` is not this sign-in's is a 400 and changes
//! nothing: a stray tab -- an old attempt's, another program's, a page
//! probing local ports -- must not end a real sign-in. The first callback
//! with the right `state` is accepted, the port stops listening, and its tab
//! is answered once the caller says how the sign-in ended.
//!
//! Everything that decides what a request means is in [`classify_request`],
//! which touches no socket, so it can be checked against the shared vectors
//! on its own. The listener is a plain `tokio` socket with a small reader: it
//! writes nothing to a log, so the code and the state in a request line never
//! reach one.
//!
//! [`LoopbackListener`] owns its port: dropping it -- a sign-in whose future
//! was dropped, for one -- closes the port at once and aborts every
//! connection it was serving.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use url::form_urlencoded;

use crate::native_auth::LOOPBACK_CALLBACK_PATH;

/// What the tab shows when the sign-in worked.
pub(crate) const SIGNED_IN_TEXT: &str = "Signed in to Thalovant. You can close this tab.";
/// What the tab shows when it did not: denied, failed, the exchange refused,
/// or the sign-in over before the tab was answered.
pub(crate) const NOT_COMPLETED_TEXT: &str = "Sign-in was not completed.";
/// What a tab whose answer belongs to another sign-in shows.
const WRONG_STATE_TEXT: &str = "This answer does not belong to the sign-in in progress.";
/// What anything other than `GET /callback` shows.
const NOT_FOUND_TEXT: &str = "Not found.";
/// Sent with every answer. The pages load nothing, and nothing they show may
/// be cached or leak the callback's address (code and state) as a referrer.
pub(crate) const PAGE_HEADERS: [(&str, &str); 5] = [
    ("Content-Type", "text/html; charset=utf-8"),
    ("Cache-Control", "no-store"),
    ("Referrer-Policy", "no-referrer"),
    ("Content-Security-Policy", "default-src 'none'"),
    ("X-Content-Type-Options", "nosniff"),
];
/// The largest request head read, terminator included; a callback's is a
/// few hundred bytes.
const MAX_REQUEST_HEAD: usize = 16 * 1024;
/// How long one connection may take to send its request head.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an accepted tab waits for its page while the code is exchanged.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long closing waits for an accepted tab's page to be written.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// How many connections may wait to be accepted.
const BACKLOG: u32 = 64;

/// What the person is told before the browser opens. Opening a browser can
/// fail without a word, so the URL is always shown.
pub(crate) fn prompt_text(url: &str, open_browser: bool) -> String {
    if open_browser {
        format!("Opening your browser to sign in to Thalovant. If it does not open, visit:\n{url}")
    } else {
        format!("To sign in to Thalovant, visit:\n{url}")
    }
}

/// What an accepted callback means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallbackOutcome {
    /// The person approved: exactly one non-empty `code`.
    Code,
    /// `error=access_denied`, exactly one value.
    Denied,
    /// Any other `error`, or neither an error nor exactly one code.
    Failed,
}

/// An accepted callback: the browser came back with this sign-in's state.
///
/// `Debug` never prints the code.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Callback {
    pub(crate) outcome: CallbackOutcome,
    /// The authorization code, when the person approved.
    pub(crate) code: Option<String>,
    /// The `error` the page sent, when it sent a non-empty one.
    pub(crate) error: Option<String>,
}

impl fmt::Debug for Callback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Callback")
            .field("outcome", &self.outcome)
            .field("code", &self.code.as_ref().map(|_| crate::redact::REDACTED))
            .field("error", &self.error)
            .finish()
    }
}

/// What the listener answers one request with, and the callback it accepts.
///
/// `(404, None)` for anything but `GET` on exactly `/callback` (another
/// path, a trailing slash, another case, `POST`, `HEAD`); `(400, None)` when
/// `state` is missing, given more than once, or not this sign-in's (compared
/// in constant time, as UTF-8 bytes); otherwise `(200, callback)`. An `error`
/// wins over a `code` sent with it: a refusal that also carries a code is
/// still a refusal. Query values are decoded once, the way a form decodes
/// them; `error_description` is not read.
pub(crate) fn classify_request(method: &str, target: &str, state: &str) -> (u16, Option<Callback>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method != "GET" || path != LOOPBACK_CALLBACK_PATH {
        return (404, None);
    }
    let (mut states, mut errors, mut codes) = (Vec::new(), Vec::new(), Vec::new());
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "state" => states.push(value.into_owned()),
            "error" => errors.push(value.into_owned()),
            "code" => codes.push(value.into_owned()),
            _ => {}
        }
    }
    let ours = match states.as_slice() {
        [given] => bool::from(given.as_bytes().ct_eq(state.as_bytes())),
        _ => false,
    };
    if !ours {
        return (400, None);
    }
    if let Some(first) = errors.first() {
        let callback = if errors.len() == 1 && first == "access_denied" {
            Callback {
                outcome: CallbackOutcome::Denied,
                code: None,
                error: Some(first.clone()),
            }
        } else {
            Callback {
                outcome: CallbackOutcome::Failed,
                code: None,
                error: Some(first.clone()).filter(|error| !error.is_empty()),
            }
        };
        return (200, Some(callback));
    }
    let callback = match codes.as_slice() {
        [code] if !code.is_empty() => Callback {
            outcome: CallbackOutcome::Code,
            code: Some(code.clone()),
            error: None,
        },
        _ => Callback {
            outcome: CallbackOutcome::Failed,
            code: None,
            error: None,
        },
    };
    (200, Some(callback))
}

/// A page that loads nothing: no script, no style sheet, no image, no link.
fn page(text: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Thalovant</title></head><body><p>{text}</p></body></html>\n"
    )
}

/// A whole HTTP/1.1 answer: the status, [`PAGE_HEADERS`], and a small page.
pub(crate) fn response_bytes(status: u16, text: &str) -> Vec<u8> {
    let body = page(text);
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let mut answer = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in PAGE_HEADERS {
        answer.push_str(&format!("{name}: {value}\r\n"));
    }
    answer.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    answer.into_bytes()
}

/// The method and target of a request head, or `None` when it has no request
/// line. Read as Latin-1, byte for byte, as an HTTP head is.
fn request_line(head: &[u8]) -> Option<(String, String)> {
    let end = head
        .windows(2)
        .position(|window| window == b"\r\n")
        .unwrap_or(head.len());
    let line: String = head[..end].iter().map(|&byte| char::from(byte)).collect();
    let parts: Vec<&str> = line.split(' ').collect();
    match parts.as_slice() {
        [method, target, version] if version.starts_with("HTTP/") => {
            Some((method.to_string(), target.to_string()))
        }
        _ => None,
    }
}

/// What reading a request head came to.
enum Head {
    /// The head, terminator included.
    Complete(Vec<u8>),
    /// Larger than [`MAX_REQUEST_HEAD`].
    TooLarge,
    /// The connection closed first.
    Closed,
}

async fn read_head(stream: &mut TcpStream) -> io::Result<Head> {
    let mut head = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(Head::Closed);
        }
        // The terminator can straddle two reads.
        let from = head.len().saturating_sub(3);
        head.extend_from_slice(&buffer[..read]);
        if let Some(at) = head[from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let end = from + at + 4;
            if end > MAX_REQUEST_HEAD {
                return Ok(Head::TooLarge);
            }
            head.truncate(end);
            return Ok(Head::Complete(head));
        }
        if head.len() >= MAX_REQUEST_HEAD {
            return Ok(Head::TooLarge);
        }
    }
}

/// What the listener and the connections it serves share.
struct Shared {
    state: String,
    /// Taken by the first connection that brings a callback with the right
    /// state; every later one finds it gone.
    accept: Mutex<Option<oneshot::Sender<Callback>>>,
    /// How the sign-in ended: `None` while it has not.
    answer: watch::Receiver<Option<bool>>,
    /// Fired once the accepted tab has its page.
    answered: Mutex<Option<oneshot::Sender<()>>>,
}

/// One connection: one request head read, one answer, closed.
async fn serve(mut stream: TcpStream, shared: Arc<Shared>) {
    let head = match tokio::time::timeout(REQUEST_READ_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(Head::Complete(head))) => head,
        Ok(Ok(Head::TooLarge)) => {
            reply(&mut stream, 400, WRONG_STATE_TEXT).await;
            return;
        }
        // Closed, broken or too slow: that connection's alone.
        _ => return,
    };
    let Some((method, target)) = request_line(&head) else {
        reply(&mut stream, 400, WRONG_STATE_TEXT).await;
        return;
    };
    let (status, callback) = classify_request(&method, &target, &shared.state);
    let Some(callback) = callback else {
        let text = if status == 404 {
            NOT_FOUND_TEXT
        } else {
            WRONG_STATE_TEXT
        };
        reply(&mut stream, status, text).await;
        return;
    };
    let first = shared
        .accept
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(first) = first else {
        // A second answer with the right state, after the first was
        // accepted, is not this sign-in's any more either.
        reply(&mut stream, 400, WRONG_STATE_TEXT).await;
        return;
    };
    let signed_in = if first.send(callback).is_ok() {
        let mut answer = shared.answer.clone();
        let waited = tokio::time::timeout(ANSWER_TIMEOUT, answer.wait_for(Option::is_some)).await;
        let ended = match waited {
            Ok(Ok(ended)) => *ended,
            // No answer within the bound, or the sign-in dropped: not signed in.
            _ => None,
        };
        ended.unwrap_or(false)
    } else {
        // Nobody waits any more: the sign-in ended before this tab came back.
        false
    };
    let text = if signed_in {
        SIGNED_IN_TEXT
    } else {
        NOT_COMPLETED_TEXT
    };
    reply(&mut stream, 200, text).await;
    let done = shared
        .answered
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(done) = done {
        let _ = done.send(());
    }
}

/// Write one answer and close the connection; a tab that went away changes
/// nothing.
async fn reply(stream: &mut TcpStream, status: u16, text: &str) {
    let _ = stream.write_all(&response_bytes(status, text)).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

/// No other socket may share the port: on Windows a socket bound without
/// `SO_EXCLUSIVEADDRUSE` can be joined by one that sets `SO_REUSEADDR`.
#[cfg(windows)]
fn exclusive_address_use(socket: &TcpSocket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
    };
    let enabled: i32 = 1;
    // SAFETY: the socket is open for the whole call, and the option value is
    // a live `i32` whose size is passed beside it.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&enabled as *const i32).cast::<u8>(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits on `127.0.0.1` for the browser to bring this sign-in's answer back.
///
/// [`LoopbackListener::bind`] binds the port (before any URL is built),
/// [`LoopbackListener::wait`] returns the accepted [`Callback`] or `None` at
/// the deadline, [`LoopbackListener::finish`] says which page its tab gets,
/// and [`LoopbackListener::close`] stops everything. Never binds anything but
/// the IPv4 loopback address.
pub(crate) struct LoopbackListener {
    /// `None` once a callback was accepted or the listener closed: the port
    /// is then closed, and a new connection is refused.
    listener: Option<TcpListener>,
    address: SocketAddr,
    shared: Arc<Shared>,
    accepted: Option<oneshot::Receiver<Callback>>,
    answer: watch::Sender<Option<bool>>,
    answered: Option<oneshot::Receiver<()>>,
    /// Whether a tab brought the accepted callback and is owed its page.
    tab_waiting: bool,
    connections: JoinSet<()>,
}

impl LoopbackListener {
    /// Bind `127.0.0.1` on a port the system picks.
    pub(crate) async fn bind(state: String) -> io::Result<Self> {
        let socket = TcpSocket::new_v4()?;
        #[cfg(windows)]
        exclusive_address_use(&socket)?;
        socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let listener = socket.listen(BACKLOG)?;
        let address = listener.local_addr()?;
        let (accept, accepted) = oneshot::channel();
        let (answer, answer_receiver) = watch::channel(None);
        let (done, answered) = oneshot::channel();
        Ok(Self {
            listener: Some(listener),
            address,
            shared: Arc::new(Shared {
                state,
                accept: Mutex::new(Some(accept)),
                answer: answer_receiver,
                answered: Mutex::new(Some(done)),
            }),
            accepted: Some(accepted),
            answer,
            answered: Some(answered),
            tab_waiting: false,
            connections: JoinSet::new(),
        })
    }

    /// The port bound.
    pub(crate) fn port(&self) -> u16 {
        self.address.port()
    }

    /// The first accepted callback, or `None` once `timeout` has passed.
    ///
    /// Connections are accepted while this runs, each served on its own
    /// task, so a connection that never sends anything holds nobody up.
    /// Measured on the monotonic clock and never cut short: a timer that
    /// wakes early waits again for what is left. A wrong-state request
    /// neither ends the wait nor restarts it. Once a callback is accepted the
    /// port stops listening. Cancel-safe: dropping the future loses nothing.
    pub(crate) async fn wait(&mut self, timeout: Duration) -> Option<Callback> {
        // A timeout too long to add is no deadline at all.
        let deadline = Instant::now().checked_add(timeout);
        enum Event {
            Accepted(Option<Callback>),
            Connection(io::Result<(TcpStream, SocketAddr)>),
            Tick,
        }
        loop {
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    Some(remaining)
                }
                None => None,
            };
            let event = {
                let (Some(listener), Some(accepted)) =
                    (self.listener.as_ref(), self.accepted.as_mut())
                else {
                    return None;
                };
                let tick = async {
                    match remaining {
                        Some(remaining) => tokio::time::sleep(remaining).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    biased;
                    callback = accepted => Event::Accepted(callback.ok()),
                    connection = listener.accept() => Event::Connection(connection),
                    () = tick => Event::Tick,
                }
            };
            match event {
                Event::Accepted(callback) => {
                    self.accepted = None;
                    // The first accepted callback ends the listening; a tab
                    // already connected is still answered.
                    self.listener = None;
                    self.tab_waiting = callback.is_some();
                    return callback;
                }
                Event::Connection(Ok((stream, _))) => {
                    self.connections
                        .spawn(serve(stream, Arc::clone(&self.shared)));
                }
                Event::Connection(Err(_)) => {
                    // Out of descriptors, or a connection reset before it was
                    // accepted: pause rather than spin, and keep waiting.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Event::Tick => {}
            }
        }
    }

    /// Answer the accepted tab: signed in, or not completed. Only the first
    /// call counts.
    pub(crate) fn finish(&self, signed_in: bool) {
        self.answer.send_if_modified(|ended| {
            if ended.is_some() {
                return false;
            }
            *ended = Some(signed_in);
            true
        });
    }

    /// Stop listening, let an accepted tab have its page, and close every
    /// other connection without waiting for it.
    pub(crate) async fn close(&mut self) {
        self.finish(false);
        self.listener = None;
        if let Some(mut accepted) = self.accepted.take() {
            // A callback accepted after the wait ended still owes its tab a page.
            self.tab_waiting |= accepted.try_recv().is_ok();
        }
        if let Some(answered) = self.answered.take() {
            if self.tab_waiting {
                // Only the accepted tab is waited for. A browser also opens
                // connections it never sends anything on; those are just closed.
                let _ = tokio::time::timeout(CLOSE_GRACE, answered).await;
            }
        }
        self.connections.shutdown().await;
    }
}

#[cfg(test)]
#[path = "loopback_tests.rs"]
mod tests;
