//! HTTP/1.1 on the loopback socket: one request a connection, a fixed pool
//! of workers, and a queue in front of each class of work.
//!
//! A herd of seats is many short clients, and a status line gives up after
//! 300 ms. Each connection costs one descriptor until it is answered. The
//! accept loop never ends: with no descriptor left, it waits and accepts
//! again. Reads run before writes. A full queue, or memory pressure, is
//! answered 503 before the request runs.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{sync_channel, Receiver, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::admit::{self, Arrival, Class, Control, Decision, Next};

/// The longest a request line and its headers may be.
pub const MAX_HEAD: usize = 64 * 1024;
/// The largest body a request may carry: a day's transcript fits.
pub const MAX_BODY: usize = 64 * 1024 * 1024;
/// How long a connection may take to send its whole request, and how long
/// it may stop reading the answer, before the worker drops it.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the accept loop waits after a failed accept before it accepts
/// again. A client that reset, or a signal, is passed over at once.
pub const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// How long a connection the queue has no room for may take to send its
/// request line before it is answered anyway.
pub const SHED_WAIT: Duration = Duration::from_millis(10);

/// A request method. The surface routes these three; anything else is a
/// 404.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Other(String),
}

impl Method {
    fn parse(raw: &str) -> Self {
        match raw {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            other => Self::Other(other.to_string()),
        }
    }
}

/// A request as read off the socket.
#[derive(Debug)]
pub struct Request {
    pub method: Method,
    /// The path and query, as sent.
    pub target: String,
    /// The `Authorization` header, when the client sent one.
    pub authorization: Option<String>,
    /// When the client needs an answer, as unix milliseconds. Past this
    /// the request is not run. Absent when the client did not say.
    pub deadline_ms: Option<u64>,
    pub body: Vec<u8>,
}

/// Writes a body as it is produced.
type BodyWriter = Box<dyn FnMut(&mut dyn Write) -> io::Result<()> + Send>;

/// An answer: a status, a content type and the bytes.
pub struct Response {
    pub code: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// `Retry-After` when the answer is a shed.
    pub retry_after: Option<u64>,
    /// When set, the body is written as it is produced and `body` is empty.
    /// A listing uses this so the worker does not hold the whole answer twice.
    pub stream: Option<BodyWriter>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("code", &self.code)
            .field("content_type", &self.content_type)
            .field("body_len", &self.body.len())
            .field("retry_after", &self.retry_after)
            .field("stream", &self.stream.is_some())
            .finish()
    }
}

impl Response {
    #[must_use]
    pub fn json(code: u16, body: String) -> Self {
        Self {
            code,
            content_type: "application/json",
            body: body.into_bytes(),
            retry_after: None,
            stream: None,
        }
    }

    #[must_use]
    pub fn text(code: u16, body: &str) -> Self {
        Self {
            code,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
            retry_after: None,
            stream: None,
        }
    }
}

/// Why a request could not be read: the client left, or sent something
/// this server answers with a status rather than a route.
#[derive(Debug)]
enum ReadError {
    Gone,
    Refuse(u16, &'static str),
}

impl From<io::Error> for ReadError {
    fn from(_: io::Error) -> Self {
        Self::Gone
    }
}

/// A connection waiting for a worker, and when it was accepted.
struct Waiting {
    stream: TcpStream,
    accepted: Instant,
}

/// A request already read, waiting for a dear slot or holding one.
struct Held {
    stream: TcpStream,
    accepted: Instant,
    request: Request,
}

/// How many searches or writes may run at once, together.
///
/// One worker stays out of that work when the pool has more than one, so
/// health, status and the workspace list still answer during a burst. A
/// single worker cannot split.
pub(crate) fn dear_limit(workers: usize) -> usize {
    admit::slots(workers)
}

/// Health, status, stats, the workspace list and the listing without
/// vectors that a status line reads. Everything else can encode, scan or
/// take the write lock, and waits on [`dear_limit`]. A listing as of a past
/// time is not cheap: it scans the database.
pub(crate) fn is_cheap(method: &Method, target: &str) -> bool {
    if !matches!(method, Method::Get) {
        return false;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/health"
        | "/stats"
        | "/metrics"
        | "/v1/status"
        | "/v1/workspaces"
        | "/__inside_memd/health" => true,
        "/v1/atoms" => {
            query.split('&').any(|pair| pair == "embedding=omit")
                && !query.split('&').any(|pair| pair.starts_with("as_of="))
        }
        _ => false,
    }
}

/// Which queue a request waits on. Maintenance is compaction and the
/// sweeps that rebuild a workspace; a write is anything else with a body.
fn classify(method: &Method, target: &str) -> Class {
    let path = target.split('?').next().unwrap_or(target);
    if matches!(method, Method::Post)
        && matches!(path, "/v1/compact" | "/v1/consolidate" | "/v1/sweep")
    {
        return Class::Maintenance;
    }
    if matches!(method, Method::Post | Method::Put) {
        return Class::Write;
    }
    Class::Read
}

/// The admission state the accept loop and the workers share.
#[derive(Clone)]
pub(crate) struct Gate {
    inner: Arc<Mutex<Control<Held>>>,
    workers: usize,
    queue: usize,
}

impl Gate {
    pub(crate) fn new(workers: usize, queue: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Control::new(workers, queue))),
            workers,
            queue,
        }
    }

    /// Use this pressure reading and do not consult the host.
    #[cfg(test)]
    pub(crate) fn pin_pressure(&self, psi: admit::Psi) {
        self.lock().pin_pressure(psi);
    }

    /// Share the append log with admission, so a full log sheds.
    pub(crate) fn watch_log(&self, log: std::sync::Arc<admit::LogUsage>) {
        self.lock().watch_log(log);
    }

    pub(crate) fn snapshot(&self) -> admit::Snapshot {
        self.lock().snapshot()
    }

    #[must_use]
    pub(crate) fn busy(&self) -> bool {
        self.snapshot().busy
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Control<Held>> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn arrive(&self, held: Held) -> Decision<Held> {
        let class = classify(&held.request.method, &held.request.target);
        let arrival = Arrival {
            class,
            seat: admit::workspace_of(&held.request.target, &held.request.body),
            deadline_ms: held.request.deadline_ms,
            bytes: held.request.body.len(),
            body: held,
        };
        self.lock().arrive(arrival, admit::unix_ms())
    }

    fn finish(&self, class: Class, sample: Option<Duration>) {
        self.lock().finish(class, sample);
    }

    fn take(&self) -> Next<Held> {
        self.lock().take(admit::unix_ms())
    }

    fn note_accept_shed(&self) {
        self.lock().note_accept_shed();
    }
}

/// Serve `listener` until the process ends.
///
/// `workers` threads answer; at most `queue` accepted connections wait for
/// them, and a connection past that is answered busy by the accept loop
/// itself. Reads, writes and maintenance that find no slot wait in their
/// own queues. `handle` runs once a request, and a panic inside it is that
/// request's 500, not the worker's end.
pub fn serve<H>(listener: &TcpListener, workers: usize, queue: usize, handle: H)
where
    H: Fn(&Request) -> Response + Send + Sync + 'static,
{
    serve_with(listener, Gate::new(workers, queue), handle);
}

/// [`serve`] on a gate the caller can read. `/stats` is that reading.
pub(crate) fn serve_with<H>(listener: &TcpListener, gate: Gate, handle: H)
where
    H: Fn(&Request) -> Response + Send + Sync + 'static,
{
    let handle = Arc::new(handle);
    let (tx, rx) = sync_channel::<Waiting>(gate.queue.max(1));
    let rx = Arc::new(Mutex::new(rx));
    let slots = gate.workers.max(1);
    for n in 0..slots {
        let rx = Arc::clone(&rx);
        let handle = Arc::clone(&handle);
        let gate = gate.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("packsetd-worker-{n}"))
            .spawn(move || work(&rx, &gate, handle.as_ref()));
        if let Err(e) = spawned {
            eprintln!("packsetd: cannot start worker {n}: {e}");
        }
    }
    let mut refused_since = None::<Instant>;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(since) = refused_since.take() {
                    eprintln!(
                        "packsetd: accepting again after {:.1}s without descriptors",
                        since.elapsed().as_secs_f64()
                    );
                }
                let waiting = Waiting {
                    stream,
                    accepted: Instant::now(),
                };
                match tx.try_send(waiting) {
                    Ok(()) => {}
                    Err(TrySendError::Full(waiting)) => {
                        gate.note_accept_shed();
                        shed(&waiting.stream);
                    }
                    Err(TrySendError::Disconnected(waiting)) => shed(&waiting.stream),
                }
            }
            // A client that reset before it was accepted, or a signal: the
            // next connection is unaffected.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                if refused_since.is_none() {
                    let why = if starved(&e) {
                        "waiting for descriptors"
                    } else {
                        "trying again"
                    };
                    eprintln!("packsetd: accept: {e}; {why}");
                    refused_since = Some(Instant::now());
                }
                std::thread::sleep(ACCEPT_BACKOFF);
            }
        }
    }
}

/// Whether an accept failed with no descriptor, buffer or memory left. A
/// closed connection gives one back.
fn starved(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc_errno::EMFILE | libc_errno::ENFILE | libc_errno::ENOBUFS | libc_errno::ENOMEM)
    )
}

/// The errno values [`starved`] reads, without a libc dependency. Linux and
/// the BSDs share three of them; `ENOBUFS` is 105 on Linux and 55 on the
/// BSDs.
mod libc_errno {
    pub const ENOMEM: i32 = 12;
    pub const ENFILE: i32 = 23;
    pub const EMFILE: i32 = 24;
    #[cfg(target_os = "linux")]
    pub const ENOBUFS: i32 = 105;
    #[cfg(not(target_os = "linux"))]
    pub const ENOBUFS: i32 = 55;
}

/// One worker: take the oldest waiting connection, answer it, repeat.
///
/// A cheap request runs on the worker that read it. Anything else runs
/// only while its class has a slot; otherwise it waits in that class's
/// queue and this worker goes back for the next connection, so health is
/// not stuck behind a search. A full queue answers busy.
fn work<H>(rx: &Mutex<Receiver<Waiting>>, gate: &Gate, handle: &H)
where
    H: Fn(&Request) -> Response,
{
    loop {
        let next = {
            let Ok(guard) = rx.lock() else {
                return;
            };
            guard.recv()
        };
        let Ok(waiting) = next else {
            return;
        };
        let request = match read_request(&waiting.stream) {
            Err(ReadError::Gone) => continue,
            Err(ReadError::Refuse(code, why)) => {
                reply(
                    &waiting.stream,
                    &mut Response::json(code, serde_json::json!({ "error": why }).to_string()),
                );
                continue;
            }
            Ok(request) => request,
        };
        if is_cheap(&request.method, &request.target) {
            let _ = reply_ran(&waiting.stream, waiting.accepted, &request, handle);
            continue;
        }
        let held = Held {
            stream: waiting.stream,
            accepted: waiting.accepted,
            request,
        };
        match gate.arrive(held) {
            Decision::Run {
                class,
                body,
                pressure_append,
            } => run_admitted(class, body, pressure_append, gate, handle),
            Decision::Queued => {}
            Decision::Shed {
                body,
                retry_after,
                reason,
            } => refuse(&body.stream, retry_after, reason),
            Decision::Expired(body) => reply_expired(&body.stream),
        }
    }
}

/// Run `held`, then the next admitted request, until the queues have
/// nothing this worker can start.
fn run_admitted<H>(
    mut class: Class,
    mut held: Held,
    mut pressure_append: bool,
    gate: &Gate,
    handle: &H,
) where
    H: Fn(&Request) -> Response,
{
    loop {
        let _pressure = pressure_append.then(admit::PressureGuard::enter);
        let sample = dispatch(&held, handle);
        drop(_pressure);
        gate.finish(class, sample);
        loop {
            match gate.take() {
                Next::Run {
                    class: next,
                    body,
                    pressure_append: append,
                } => {
                    class = next;
                    held = body;
                    pressure_append = append;
                    break;
                }
                Next::Expired(body) => reply_expired(&body.stream),
                Next::Idle => return,
            }
        }
    }
}

/// Run `held` unless its client has gone or its deadline has passed.
/// The duration is how long it ran, when it did.
fn dispatch<H>(held: &Held, handle: &H) -> Option<Duration>
where
    H: Fn(&Request) -> Response,
{
    if admit::deadline_passed(held.request.deadline_ms, admit::unix_ms()) {
        reply_expired(&held.stream);
        return None;
    }
    let started = Instant::now();
    if reply_ran(&held.stream, held.accepted, &held.request, handle) {
        Some(started.elapsed())
    } else {
        None
    }
}

/// Run `request` unless its client has gone, write the answer and close.
/// False when the client had already gone and the handler did not run.
fn reply_ran<H>(stream: &TcpStream, accepted: Instant, request: &Request, handle: &H) -> bool
where
    H: Fn(&Request) -> Response,
{
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_nodelay(true);
    // A client that timed out and closed while its request waited already
    // has its error, so the request is not run: a write it gave up on is
    // not applied.
    if abandoned(stream) {
        return false;
    }
    let mut response = run(request, accepted, handle);
    // A listing is the bytes of every atom. Give that buffer back once it
    // has been written, or the worker's arena keeps the high-water mark.
    let heavy = response.body.len() >= 64 * 1024 || response.stream.is_some();
    reply(stream, &mut response);
    drop(response);
    if heavy {
        crate::store::release_heap();
    }
    true
}

fn reply(stream: &TcpStream, response: &mut Response) {
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let _ = write_response(stream, response, &[]);
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// Run the handler, turning a panic into a 500 and a log line that names
/// the route.
fn run<H>(request: &Request, accepted: Instant, handle: &H) -> Response
where
    H: Fn(&Request) -> Response,
{
    let started = Instant::now();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(request))) {
        Ok(response) => response,
        Err(cause) => {
            let what = cause
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| cause.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".to_string());
            eprintln!(
                "packsetd: {:?} {} failed after {:.0} ms in queue and {:.0} ms running: {what}",
                request.method,
                request.target.split('?').next().unwrap_or(""),
                started.duration_since(accepted).as_secs_f64() * 1000.0,
                started.elapsed().as_secs_f64() * 1000.0
            );
            Response::json(
                500,
                serde_json::json!({ "error": format!("packsetd failed on this request: {what}") })
                    .to_string(),
            )
        }
    }
}

/// Whether the client closed its end after sending: a read that finds the
/// end of the stream rather than nothing yet.
fn abandoned(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let gone = match (&*stream).read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::Interrupted,
    };
    let _ = stream.set_nonblocking(false);
    gone
}

/// Answer a connection the queue has no room for, from the accept loop: a
/// health probe still learns the writer is up, and anything else is told
/// to try again in a second. A request line still on its way after
/// [`SHED_WAIT`] is not waited for.
fn shed(stream: &TcpStream) {
    let _ = stream.set_write_timeout(Some(SHED_WAIT));
    // Read the request line, so closing does not reset a request the kernel
    // already holds, and so a probe can be told apart.
    let mut reader = Deadline::after(stream, SHED_WAIT);
    let mut head = [0u8; 1024];
    let mut seen = 0;
    while seen < head.len() && !head[..seen].windows(2).any(|w| w == b"\r\n") {
        match reader.read(&mut head[seen..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen += n,
        }
    }
    let probe =
        head[..seen].starts_with(b"GET /health ") || head[..seen].starts_with(b"GET /stats ");
    let mut response = if probe {
        Response::text(200, "packsetd ok, busy")
    } else {
        busy("the connection queue is full; try again")
    };
    let _ = write_response(stream, &mut response, &[("Retry-After", "1")]);
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// Answer `stream` with 503 and `Retry-After`. None of the request ran.
fn refuse(stream: &TcpStream, retry_after: u64, reason: &str) {
    let secs = retry_after.max(1).to_string();
    let _ = write_response(stream, &mut busy(reason), &[("Retry-After", &secs)]);
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// The deadline passed while the request was waiting. It did not run.
fn reply_expired(stream: &TcpStream) {
    reply(
        stream,
        &mut Response::json(
            408,
            r#"{"error":"the deadline passed before this request ran"}"#.to_string(),
        ),
    );
}

/// The answer to a request this writer has no room for: none of it ran, so
/// the client may send it again.
fn busy(reason: &str) -> Response {
    Response::json(503, format!(r#"{{"error":"packsetd is busy: {reason}"}}"#))
}

fn reason(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn write_response(
    mut stream: &TcpStream,
    response: &mut Response,
    extra: &[(&str, &str)],
) -> io::Result<()> {
    if response.stream.is_some() {
        return write_chunked(stream, response, extra);
    }
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.code,
        reason(response.code),
        response.content_type,
        response.body.len()
    );
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(secs) = response.retry_after {
        head.push_str(&format!("Retry-After: {secs}\r\n"));
    }
    head.push_str("\r\n");
    // The head and the body stay apart. Joining them kept a second copy of
    // a listing until the write finished.
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// Chunked transfer of a body that is produced as it is written. Each
/// chunk is at most 16 KiB, so a listing does not accumulate.
fn write_chunked(
    mut stream: &TcpStream,
    response: &mut Response,
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let Some(mut produce) = response.stream.take() else {
        return Ok(());
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        response.code,
        reason(response.code),
        response.content_type
    );
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(secs) = response.retry_after {
        head.push_str(&format!("Retry-After: {secs}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    let mut chunked = Chunked::new(stream);
    produce(&mut chunked)?;
    chunked.finish()?;
    stream.flush()
}

/// Buffers up to 16 KiB, then writes one HTTP chunk.
struct Chunked<W> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> Chunked<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(16 * 1024),
        }
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        write!(self.inner, "{:x}\r\n", self.buf.len())?;
        self.inner.write_all(&self.buf)?;
        self.inner.write_all(b"\r\n")?;
        self.buf.clear();
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.inner.write_all(b"0\r\n\r\n")?;
        Ok(())
    }
}

impl<W: Write> Write for Chunked<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 16 * 1024 {
            self.flush_buf()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.inner.flush()
    }
}

/// Reads with one deadline for the whole request. Each read waits only for
/// the time left, so a client that sends a byte at a time still runs out.
struct Deadline<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl<'a> Deadline<'a> {
    fn after(stream: &'a TcpStream, wait: Duration) -> Self {
        Self {
            stream,
            until: Instant::now() + wait,
        }
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left))?;
        (&*self.stream).read(buf)
    }
}

/// `deadline` or `deadline_ms` on the query, as unix milliseconds. The
/// header wins when both are present; this is only read when the header
/// was absent.
fn query_deadline(target: &str) -> Option<u64> {
    let (_, query) = target.split_once('?')?;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if (key == "deadline" || key == "deadline_ms") && !value.is_empty() {
            return value.parse().ok();
        }
    }
    None
}

/// The request line, the headers this server reads, and the body, all
/// within [`IO_TIMEOUT`].
fn read_request(stream: &TcpStream) -> Result<Request, ReadError> {
    let mut reader = BufReader::new(Deadline::after(stream, IO_TIMEOUT));
    let mut head_len = 0usize;
    let mut line = String::new();
    // A blank line before the request line is allowed, and skipped.
    let request_line = loop {
        line.clear();
        let n = read_line(&mut reader, &mut line, &mut head_len)?;
        if n == 0 {
            return Err(ReadError::Gone);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if !trimmed.is_empty() {
            break trimmed.to_string();
        }
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Refuse(
            400,
            "a request line is METHOD TARGET HTTP/1.1",
        ));
    };
    if !version.starts_with("HTTP/1.") || target.is_empty() {
        return Err(ReadError::Refuse(
            400,
            "a request line is METHOD TARGET HTTP/1.1",
        ));
    }
    let mut length: Option<usize> = None;
    let mut chunked = false;
    let mut expect_continue = false;
    let mut authorization = None;
    let mut deadline_ms = None;
    loop {
        line.clear();
        let n = read_line(&mut reader, &mut line, &mut head_len)?;
        if n == 0 {
            return Err(ReadError::Gone);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(ReadError::Refuse(400, "a header line is NAME: VALUE"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed: usize = value
                .parse()
                .map_err(|_| ReadError::Refuse(400, "Content-Length is not a number"))?;
            if length.is_some_and(|seen| seen != parsed) {
                return Err(ReadError::Refuse(
                    400,
                    "two Content-Length headers disagree",
                ));
            }
            length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value
                .split(',')
                .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"));
            if !chunked {
                return Err(ReadError::Refuse(
                    501,
                    "only the chunked transfer coding is read",
                ));
            }
        } else if name.eq_ignore_ascii_case("expect") && value.eq_ignore_ascii_case("100-continue")
        {
            expect_continue = true;
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-packset-deadline") && deadline_ms.is_none() {
            deadline_ms = value.parse().ok();
        }
    }
    if deadline_ms.is_none() {
        deadline_ms = query_deadline(target);
    }
    let wants_body = chunked || length.is_some_and(|n| n > 0);
    if length.is_some_and(|n| n > MAX_BODY) {
        return Err(ReadError::Refuse(
            413,
            "the body is larger than packsetd reads",
        ));
    }
    if expect_continue && wants_body {
        let mut out = stream;
        out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        out.flush()?;
    }
    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        let n = length.unwrap_or(0);
        let mut body = vec![0u8; n];
        reader.read_exact(&mut body)?;
        body
    };
    Ok(Request {
        method: Method::parse(method),
        target: target.to_string(),
        authorization,
        deadline_ms,
        body,
    })
}

/// One line of the head, refused once the head passes [`MAX_HEAD`].
fn read_line(
    reader: &mut BufReader<Deadline<'_>>,
    line: &mut String,
    head_len: &mut usize,
) -> Result<usize, ReadError> {
    let mut raw = Vec::new();
    let n = reader
        .by_ref()
        .take((MAX_HEAD - *head_len + 1) as u64)
        .read_until(b'\n', &mut raw)?;
    *head_len += n;
    if *head_len > MAX_HEAD {
        return Err(ReadError::Refuse(
            431,
            "the request head is larger than packsetd reads",
        ));
    }
    line.push_str(&String::from_utf8_lossy(&raw));
    Ok(n)
}

/// A chunked body: hex sizes, each chunk, a zero size, then trailers.
fn read_chunked(reader: &mut BufReader<Deadline<'_>>) -> Result<Vec<u8>, ReadError> {
    let mut body = Vec::new();
    let mut head_len = 0usize;
    loop {
        let mut size_line = String::new();
        if read_line(reader, &mut size_line, &mut head_len)? == 0 {
            return Err(ReadError::Gone);
        }
        let size_hex = size_line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| ReadError::Refuse(400, "a chunk size is not hex"))?;
        if size == 0 {
            break;
        }
        if body.len() + size > MAX_BODY {
            return Err(ReadError::Refuse(
                413,
                "the body is larger than packsetd reads",
            ));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
        head_len = 0;
    }
    loop {
        let mut trailer = String::new();
        if read_line(reader, &mut trailer, &mut head_len)? == 0
            || trailer.trim_end_matches(['\r', '\n']).is_empty()
        {
            break;
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server on a free port with `workers` and `queue`, answering with
    /// `handle`, in a thread that lives for the test run.
    fn start<H>(workers: usize, queue: usize, handle: H) -> u16
    where
        H: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        let listener = crate::test_support::bind();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || serve(&listener, workers, queue, handle));
        port
    }

    fn exchange(port: u16, raw: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(raw).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn echo(request: &Request) -> Response {
        Response::json(
            200,
            serde_json::json!({
                "method": format!("{:?}", request.method),
                "target": request.target,
                "body": String::from_utf8_lossy(&request.body),
            })
            .to_string(),
        )
    }

    #[test]
    fn a_request_is_read_routed_and_closed() {
        let port = start(2, 8, echo);
        let out = exchange(
            port,
            b"POST /v1/atoms?workspace=w HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\n\r\n{\"a\":1}",
        );
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.contains("Connection: close\r\n"), "{out}");
        assert!(out.contains(r#""target":"/v1/atoms?workspace=w""#), "{out}");
        assert!(out.contains(r#""body":"{\"a\":1}""#), "{out}");
    }

    #[test]
    fn a_chunked_body_and_an_expect_are_read() {
        let port = start(1, 8, echo);
        let out = exchange(
            port,
            b"PUT /v1/pin HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
        );
        assert!(
            out.starts_with("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK"),
            "{out}"
        );
        assert!(out.contains(r#""body":"abcde""#), "{out}");
    }

    #[test]
    fn a_bad_head_is_refused_with_a_status() {
        let port = start(1, 8, echo);
        let out = exchange(port, b"NONSENSE\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 400 "), "{out}");
        let out = exchange(port, b"POST / HTTP/1.1\r\nContent-Length: nine\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 400 "), "{out}");
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let out = exchange(port, big.as_bytes());
        assert!(out.starts_with("HTTP/1.1 413 "), "{out}");
    }

    #[test]
    fn a_panic_is_that_requests_500_and_the_worker_answers_the_next() {
        let port = start(1, 8, |request: &Request| {
            assert!(request.target != "/boom", "the handler fell over");
            Response::text(200, "fine")
        });
        let out = exchange(port, b"GET /boom HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 500 "), "{out}");
        assert!(out.contains("the handler fell over"), "{out}");
        let out = exchange(port, b"GET /next HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
    }

    #[test]
    fn a_full_queue_answers_busy_and_a_probe_still_hears_up() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let (g, r) = (Arc::clone(&gate), Arc::clone(&ran));
        let port = start(1, 1, move |_: &Request| {
            r.fetch_add(1, Ordering::SeqCst);
            let _wait = g.lock();
            Response::text(200, "done")
        });
        // One request occupies the worker and one fills the queue.
        let first = std::thread::spawn(move || exchange(port, b"GET /a HTTP/1.1\r\n\r\n"));
        while ran.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = std::thread::spawn(move || exchange(port, b"GET /b HTTP/1.1\r\n\r\n"));
        std::thread::sleep(Duration::from_millis(100));
        let busy = exchange(port, b"GET /c HTTP/1.1\r\n\r\n");
        assert!(busy.starts_with("HTTP/1.1 503 "), "{busy}");
        assert!(busy.contains("Retry-After: 1\r\n"), "{busy}");
        let probe = exchange(port, b"GET /health HTTP/1.1\r\n\r\n");
        assert!(probe.starts_with("HTTP/1.1 200 OK"), "{probe}");
        assert!(probe.contains("packsetd ok"), "{probe}");
        drop(held);
        assert!(first.join().unwrap().starts_with("HTTP/1.1 200 OK"));
        assert!(second.join().unwrap().starts_with("HTTP/1.1 200 OK"));
    }

    /// Both ends of one loopback connection, the accepted end first.
    fn pair() -> (TcpStream, TcpStream) {
        let listener = crate::test_support::bind();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    /// A client that sends one byte every `gap` until `stop` is set.
    fn trickle(mut client: TcpStream, gap: Duration, stop: Arc<AtomicUsize>) {
        std::thread::spawn(move || {
            while stop.load(Ordering::SeqCst) == 0 {
                if client.write_all(b"G").is_err() {
                    break;
                }
                std::thread::sleep(gap);
            }
        });
    }

    #[test]
    fn a_byte_at_a_time_still_runs_out_of_time() {
        let (server, client) = pair();
        let stop = Arc::new(AtomicUsize::new(0));
        trickle(client, Duration::from_millis(20), Arc::clone(&stop));
        let started = Instant::now();
        let mut reader = Deadline::after(&server, Duration::from_millis(200));
        let mut buf = [0u8; 64];
        let out = loop {
            match reader.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(_) => {}
                Err(e) => break Err(e),
            }
        };
        stop.store(1, Ordering::SeqCst);
        assert!(out.is_err(), "the reads never stopped");
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_trickled_request_line_is_shed_within_the_wait() {
        let (server, client) = pair();
        let mut answer = client.try_clone().unwrap();
        let stop = Arc::new(AtomicUsize::new(0));
        trickle(client, Duration::from_millis(3), Arc::clone(&stop));
        let started = Instant::now();
        shed(&server);
        let took = started.elapsed();
        stop.store(1, Ordering::SeqCst);
        assert!(took < SHED_WAIT * 5, "{took:?}");
        answer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut out = String::new();
        let _ = answer.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
    }

    #[test]
    fn a_request_whose_client_left_is_not_run() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let (g, r) = (Arc::clone(&gate), Arc::clone(&ran));
        let port = start(1, 8, move |_: &Request| {
            r.fetch_add(1, Ordering::SeqCst);
            let _wait = g.lock();
            Response::text(200, "done")
        });
        let first = std::thread::spawn(move || exchange(port, b"GET /a HTTP/1.1\r\n\r\n"));
        while ran.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Sent and closed while the worker is busy: a client that timed out.
        {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"GET /left HTTP/1.1\r\n\r\n").unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
        assert!(first.join().unwrap().starts_with("HTTP/1.1 200 OK"));
        let out = exchange(port, b"GET /after HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
        assert_eq!(ran.load(Ordering::SeqCst), 2, "the abandoned request ran");
    }

    #[test]
    fn parked_bodies_stay_within_one_request() {
        assert!(admit::park_body(0, MAX_BODY, 128, 0));
        assert!(!admit::park_body(MAX_BODY, 1, 128, 1));
        assert!(!admit::park_body(1, MAX_BODY, 128, 1));
        assert!(admit::park_body(0, 0, 128, 127));
        assert!(!admit::park_body(0, 0, 128, 128));
    }

    #[test]
    fn dear_slots_leave_one_worker_for_a_pool() {
        assert_eq!(dear_limit(4), 3);
        assert_eq!(dear_limit(1), 1);
        assert!(is_cheap(&Method::Get, "/health"));
        assert!(is_cheap(&Method::Get, "/stats"));
        assert!(is_cheap(&Method::Get, "/metrics"));
        assert!(is_cheap(&Method::Get, "/v1/status?workspace=seat"));
        assert!(is_cheap(&Method::Get, "/v1/workspaces"));
        assert!(is_cheap(&Method::Get, "/__inside_memd/health"));
        assert!(is_cheap(
            &Method::Get,
            "/v1/atoms?workspace=seat&embedding=omit"
        ));
        assert!(!is_cheap(&Method::Get, "/v1/atoms?workspace=seat"));
        assert!(!is_cheap(
            &Method::Get,
            "/v1/atoms?workspace=seat&embedding=omit&as_of=2026-10-01T00:00:00Z"
        ));
        assert!(!is_cheap(
            &Method::Get,
            "/v1/search?workspace=seat&q=fusion"
        ));
        assert!(!is_cheap(&Method::Post, "/health"));
    }

    /// Two workers, one dear slot, a queue of one: the third search is
    /// answered busy and never runs.
    #[test]
    fn a_search_past_the_lanes_room_is_answered_busy_and_not_run() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let ran = Arc::new(Mutex::new(Vec::new()));
        let (g, r) = (Arc::clone(&gate), Arc::clone(&ran));
        let port = start(2, 1, move |request: &Request| {
            r.lock().unwrap().push(request.target.clone());
            let _wait = g.lock();
            Response::text(200, "done")
        });
        let first =
            std::thread::spawn(move || exchange(port, b"GET /v1/search?q=a HTTP/1.1\r\n\r\n"));
        while ran.lock().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let second =
            std::thread::spawn(move || exchange(port, b"GET /v1/search?q=b HTTP/1.1\r\n\r\n"));
        std::thread::sleep(Duration::from_millis(100));
        let busy = exchange(port, b"GET /v1/search?q=c HTTP/1.1\r\n\r\n");
        assert!(busy.starts_with("HTTP/1.1 503 "), "{busy}");
        assert!(busy.contains("Retry-After: 1\r\n"), "{busy}");
        drop(held);
        assert!(first.join().unwrap().starts_with("HTTP/1.1 200 OK"));
        assert!(second.join().unwrap().starts_with("HTTP/1.1 200 OK"));
        assert_eq!(
            *ran.lock().unwrap(),
            vec!["/v1/search?q=a", "/v1/search?q=b"]
        );
    }

    /// Two workers, one dear slot. A health check returns while a search
    /// still holds that slot, and a second search does not run beside it.
    #[test]
    fn health_answers_while_a_search_holds_the_other_worker() {
        const HOLD: Duration = Duration::from_millis(400);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        let entered = Arc::new(Mutex::new(Some(tx)));
        let running_h = Arc::clone(&running);
        let peak_h = Arc::clone(&peak);
        let port = start(2, 8, move |request: &Request| {
            if is_cheap(&request.method, &request.target) {
                return Response::text(200, "ok");
            }
            let now = running_h.fetch_add(1, Ordering::SeqCst) + 1;
            peak_h.fetch_max(now, Ordering::SeqCst);
            if let Some(tx) = entered.lock().expect("entered").take() {
                let _ = tx.send(());
            }
            std::thread::sleep(HOLD);
            running_h.fetch_sub(1, Ordering::SeqCst);
            Response::text(200, "search")
        });

        let search = std::thread::spawn(move || {
            let started = Instant::now();
            let out = exchange(port, b"GET /v1/search?q=one HTTP/1.1\r\nHost: x\r\n\r\n");
            (started.elapsed(), out)
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("search started");
        let late = std::thread::spawn(move || {
            let started = Instant::now();
            let out = exchange(port, b"GET /v1/search?q=two HTTP/1.1\r\nHost: x\r\n\r\n");
            (started.elapsed(), out)
        });
        std::thread::sleep(Duration::from_millis(40));
        let health_at = Instant::now();
        let health = exchange(port, b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
        let health_wait = health_at.elapsed();
        let (first_wait, first_out) = search.join().expect("first search");
        let (late_wait, late_out) = late.join().expect("late search");
        eprintln!(
            "health_ms {:.1} holding_search_ms {:.1} parked_search_ms {:.1}",
            health_wait.as_secs_f64() * 1000.0,
            first_wait.as_secs_f64() * 1000.0,
            late_wait.as_secs_f64() * 1000.0
        );

        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        assert!(
            health_wait < Duration::from_millis(200),
            "health waited {health_wait:?} behind a search"
        );
        assert!(first_out.starts_with("HTTP/1.1 200"), "{first_out}");
        assert!(
            first_wait >= HOLD,
            "the holding search returned in {first_wait:?}"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert!(late_out.starts_with("HTTP/1.1 200"), "{late_out}");
        assert!(
            late_wait >= HOLD,
            "the parked search returned in {late_wait:?}"
        );
    }

    fn start_gate<H>(gate: Gate, handle: H) -> u16
    where
        H: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        let listener = crate::test_support::bind();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || serve_with(&listener, gate, handle));
        port
    }

    #[test]
    fn compaction_is_maintenance_and_a_remember_is_a_write() {
        assert_eq!(classify(&Method::Post, "/v1/compact"), Class::Maintenance);
        assert_eq!(
            classify(&Method::Post, "/v1/consolidate"),
            Class::Maintenance
        );
        assert_eq!(classify(&Method::Post, "/v1/sweep"), Class::Maintenance);
        assert_eq!(classify(&Method::Post, "/v1/atoms"), Class::Write);
        assert_eq!(classify(&Method::Put, "/v1/memory"), Class::Write);
        assert_eq!(
            classify(&Method::Get, "/v1/search?workspace=seat&q=a"),
            Class::Read
        );
    }

    #[test]
    fn a_deadline_that_has_passed_is_not_run() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&ran);
        let port = start(2, 8, move |_: &Request| {
            counted.fetch_add(1, Ordering::SeqCst);
            Response::text(200, "ran")
        });
        let past = admit::unix_ms().saturating_sub(1_000);
        let raw = format!("GET /v1/search?q=late HTTP/1.1\r\nX-Packset-Deadline: {past}\r\n\r\n");
        let out = exchange(port, raw.as_bytes());
        assert!(out.starts_with("HTTP/1.1 408 "), "{out}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        let future = admit::unix_ms().saturating_add(60_000);
        let raw = format!("GET /v1/search?q=soon HTTP/1.1\r\nX-Packset-Deadline: {future}\r\n\r\n");
        let out = exchange(port, raw.as_bytes());
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// The slot is held. The request behind it names a deadline that passes
    /// while it waits, so the handler never sees it.
    #[test]
    fn a_request_that_waits_past_its_deadline_is_not_run() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let (g, r) = (Arc::clone(&gate), Arc::clone(&ran));
        let port = start(2, 8, move |request: &Request| {
            r.fetch_add(1, Ordering::SeqCst);
            if request.target.contains("hold") {
                let _wait = g.lock();
            }
            Response::text(200, "ran")
        });
        let first =
            std::thread::spawn(move || exchange(port, b"GET /v1/search?q=hold HTTP/1.1\r\n\r\n"));
        while ran.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let soon = admit::unix_ms().saturating_add(80);
        let raw = format!("GET /v1/search?q=queued HTTP/1.1\r\nX-Packset-Deadline: {soon}\r\n\r\n");
        let second = std::thread::spawn(move || exchange(port, raw.as_bytes()));
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        assert!(first.join().unwrap().starts_with("HTTP/1.1 200 "));
        let late = second.join().unwrap();
        assert!(late.starts_with("HTTP/1.1 408 "), "{late}");
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the late search ran");
    }

    /// The same deadline the header carries, as a query parameter.
    #[test]
    fn a_deadline_on_the_query_is_the_one_admission_sees() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&ran);
        let port = start(2, 8, move |_: &Request| {
            counted.fetch_add(1, Ordering::SeqCst);
            Response::text(200, "ran")
        });
        let past = admit::unix_ms().saturating_sub(1_000);
        let raw = format!("GET /v1/search?q=late&deadline={past} HTTP/1.1\r\n\r\n");
        let out = exchange(port, raw.as_bytes());
        assert!(out.starts_with("HTTP/1.1 408 "), "{out}");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        let future = admit::unix_ms().saturating_add(60_000);
        let raw = format!("GET /v1/atoms?workspace=seat&deadline_ms={future} HTTP/1.1\r\n\r\n");
        let out = exchange(port, raw.as_bytes());
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        // The header wins when both name a time.
        let raw = format!(
            "GET /v1/search?q=both&deadline={future} HTTP/1.1\r\nX-Packset-Deadline: {past}\r\n\r\n"
        );
        let out = exchange(port, raw.as_bytes());
        assert!(out.starts_with("HTTP/1.1 408 "), "{out}");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// One write holds its slot. N searches, each with a deadline, answer
    /// inside that deadline or come back 503 before it. None of them wait
    /// the write out.
    #[test]
    fn a_burst_of_readers_during_a_held_write_meets_the_deadline_or_sheds() {
        const READERS: usize = 20;
        const HOLD: Duration = Duration::from_secs(2);
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let entered = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(Mutex::new(Vec::new()));
        let (g, entered_h, ran_h) = (Arc::clone(&gate), Arc::clone(&entered), Arc::clone(&ran));
        let port = start(4, 64, move |request: &Request| {
            ran_h.lock().unwrap().push(request.target.clone());
            if request.method == Method::Post {
                entered_h.fetch_add(1, Ordering::SeqCst);
                let _wait = g.lock();
                return Response::text(200, "write");
            }
            Response::text(200, "read")
        });
        let write = std::thread::spawn(move || {
            exchange(
                port,
                b"POST /v1/atoms HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
            )
        });
        let started = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the write did not start"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let allowance = Duration::from_millis(400);
        let mut joins = Vec::new();
        for i in 0..READERS {
            joins.push(std::thread::spawn(move || {
                let deadline = admit::unix_ms().saturating_add(allowance.as_millis() as u64);
                let raw = format!("GET /v1/search?q={i}&deadline={deadline} HTTP/1.1\r\n\r\n");
                let began = Instant::now();
                let out = exchange(port, raw.as_bytes());
                (began.elapsed(), out)
            }));
        }
        for join in joins {
            let (took, out) = join.join().unwrap();
            assert!(
                took < HOLD,
                "a reader waited {took:?}, as long as the held write"
            );
            if out.starts_with("HTTP/1.1 200 ") {
                assert!(
                    took < allowance + Duration::from_millis(150),
                    "a read answered after its deadline: {took:?}"
                );
            } else {
                assert!(out.starts_with("HTTP/1.1 503 "), "{out}");
                assert!(out.contains("Retry-After: "), "{out}");
                assert!(
                    took < Duration::from_millis(150),
                    "a shed read was not fast: {took:?} {out}"
                );
            }
        }
        drop(held);
        assert!(write.join().unwrap().starts_with("HTTP/1.1 200 "));
        let targets = ran.lock().unwrap();
        assert!(
            targets
                .iter()
                .any(|target| target.starts_with("/v1/search")),
            "no search ran: {targets:?}"
        );
    }

    /// Writes fill their queue and come back 503. Reads that arrive beside
    /// them still finish quickly, and the handler never runs more than the
    /// slot count.
    #[test]
    fn a_flood_of_writes_is_shed_while_reads_stay_quick() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let writes_ran = Arc::new(AtomicUsize::new(0));
        let gate = Gate::new(4, 8);
        gate.pin_pressure(admit::Psi::quiet());
        let (running_h, peak_h, writes_h) = (
            Arc::clone(&running),
            Arc::clone(&peak),
            Arc::clone(&writes_ran),
        );
        let port = start_gate(gate.clone(), move |request: &Request| {
            let now = running_h.fetch_add(1, Ordering::SeqCst) + 1;
            peak_h.fetch_max(now, Ordering::SeqCst);
            let read = request.target.contains("/v1/search");
            if read {
                std::thread::sleep(Duration::from_millis(30));
            } else {
                writes_h.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(150));
            }
            running_h.fetch_sub(1, Ordering::SeqCst);
            Response::text(200, if read { "read" } else { "write" })
        });

        let rss_before = rss_kib();
        let primed = Arc::clone(&writes_ran);
        let prime = std::thread::spawn(move || {
            exchange(
                port,
                b"POST /v1/atoms HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
            )
        });
        let started = Instant::now();
        while primed.load(Ordering::SeqCst) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the first write did not start"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut joins = Vec::new();
        for i in 0..24 {
            joins.push(std::thread::spawn(move || {
                let body = format!(r#"{{"n":{i}}}"#);
                let raw = format!(
                    "POST /v1/atoms HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let began = Instant::now();
                let out = exchange(port, raw.as_bytes());
                ("write", began.elapsed(), out)
            }));
        }
        std::thread::sleep(Duration::from_millis(80));
        for i in 0..8 {
            joins.push(std::thread::spawn(move || {
                let raw = format!("GET /v1/search?q={i} HTTP/1.1\r\n\r\n");
                let began = Instant::now();
                let out = exchange(port, raw.as_bytes());
                ("read", began.elapsed(), out)
            }));
        }

        let stop = Arc::new(AtomicUsize::new(0));
        let watch_gate = gate.clone();
        let watch_stop = Arc::clone(&stop);
        let high = Arc::new(Mutex::new((0usize, 0usize)));
        let high_w = Arc::clone(&high);
        let watcher = std::thread::spawn(move || {
            let mut max_depth = 0usize;
            let mut max_flight = 0usize;
            while watch_stop.load(Ordering::SeqCst) == 0 {
                let snap = watch_gate.snapshot();
                max_depth = max_depth.max(snap.write_depth);
                let flight = snap.read_in_flight + snap.write_in_flight + snap.maint_in_flight;
                max_flight = max_flight.max(flight);
                std::thread::sleep(Duration::from_millis(1));
            }
            *high_w.lock().unwrap() = (max_depth, max_flight);
        });

        let mut write_shed = 0usize;
        let mut read_ok = 0usize;
        let mut read_slow = Duration::ZERO;
        for join in joins {
            let (kind, took, out) = join.join().unwrap();
            if kind == "write" {
                if out.starts_with("HTTP/1.1 503 ") {
                    assert!(
                        out.contains("Retry-After: "),
                        "a shed write had no Retry-After: {out}"
                    );
                    write_shed += 1;
                } else {
                    assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
                }
            } else if out.starts_with("HTTP/1.1 200 ") {
                read_ok += 1;
                read_slow = read_slow.max(took);
            } else {
                panic!("a read was not answered: {out}");
            }
        }
        let prime_out = prime.join().unwrap();
        assert!(prime_out.starts_with("HTTP/1.1 200 "), "{prime_out}");
        stop.store(1, Ordering::SeqCst);
        watcher.join().unwrap();
        let (max_depth, max_flight) = *high.lock().unwrap();
        let rss_after = rss_kib();
        let rss_delta = rss_after.saturating_sub(rss_before);
        eprintln!(
            "overload writes_shed={write_shed} writes_ran={} reads_ok={read_ok} read_max_ms={:.1} peak_handler={} max_write_depth={max_depth} max_in_flight={max_flight} rss_delta_kib={rss_delta}",
            writes_ran.load(Ordering::SeqCst),
            read_slow.as_secs_f64() * 1000.0,
            peak.load(Ordering::SeqCst),
        );

        assert!(write_shed >= 8, "only {write_shed} writes were shed");
        assert_eq!(read_ok, 8, "a read was shed or lost");
        assert!(
            read_slow < Duration::from_millis(1_500),
            "a read took {read_slow:?}"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "handler peak {}",
            peak.load(Ordering::SeqCst)
        );
        assert!(max_depth <= 2, "write queue depth {max_depth}");
        assert!(max_flight <= 3, "in flight {max_flight}");
        if rss_before > 0 {
            assert!(rss_delta < 64 * 1024, "rss grew by {rss_delta} KiB");
        }
    }

    #[cfg(target_os = "linux")]
    fn rss_kib() -> u64 {
        let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
            return 0;
        };
        text.lines()
            .find_map(|line| {
                let rest = line.strip_prefix("VmRSS")?.strip_prefix(':')?;
                rest.split_whitespace().next()?.parse().ok()
            })
            .unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    fn rss_kib() -> u64 {
        0
    }
}
