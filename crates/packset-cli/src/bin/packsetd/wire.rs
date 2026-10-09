//! HTTP/1.1 on the loopback socket: one request a connection, a fixed pool
//! of workers, and a bounded queue in front of them.
//!
//! A herd of seats is many short clients, and a status line gives up after
//! 300 ms. Each connection costs one descriptor until it is answered. The
//! accept loop never ends: with no descriptor left, it waits and accepts
//! again.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{sync_channel, Receiver, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    pub body: Vec<u8>,
}

/// An answer: a status, a content type and the bytes.
#[derive(Debug)]
pub struct Response {
    pub code: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub fn json(code: u16, body: String) -> Self {
        Self {
            code,
            content_type: "application/json",
            body: body.into_bytes(),
        }
    }

    #[must_use]
    pub fn text(code: u16, body: &str) -> Self {
        Self {
            code,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
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

/// How many searches or writes may run at once.
///
/// One worker stays out of that work when the pool has more than one, so
/// health, status and the workspace list still answer during a burst. A
/// single worker cannot split.
pub(crate) fn dear_limit(workers: usize) -> usize {
    match workers {
        0 | 1 => 1,
        n => n - 1,
    }
}

/// Health, status and the workspace list. Everything else can encode, scan
/// or take the write lock, and waits on [`dear_limit`].
pub(crate) fn is_cheap(method: &Method, target: &str) -> bool {
    if !matches!(method, Method::Get) {
        return false;
    }
    let path = target.split('?').next().unwrap_or(target);
    matches!(
        path,
        "/health" | "/v1/status" | "/v1/workspaces" | "/__inside_memd/health"
    )
}

/// Searches and writes in flight, and the ones waiting for a slot.
struct Lane {
    limit: usize,
    in_flight: usize,
    parked: VecDeque<Held>,
}

impl Lane {
    fn new(workers: usize) -> Self {
        Self {
            limit: dear_limit(workers),
            in_flight: 0,
            parked: VecDeque::new(),
        }
    }

    /// Run `job` now, or park it when the dear slots are full.
    fn begin(&mut self, job: Held) -> Option<Held> {
        if self.in_flight < self.limit {
            self.in_flight += 1;
            Some(job)
        } else {
            self.parked.push_back(job);
            None
        }
    }

    /// The next parked job, which keeps this slot. `None` releases it.
    fn end(&mut self) -> Option<Held> {
        if let Some(next) = self.parked.pop_front() {
            Some(next)
        } else {
            self.in_flight = self.in_flight.saturating_sub(1);
            None
        }
    }
}

/// Serve `listener` until the process ends.
///
/// `workers` threads answer; at most `queue` accepted connections wait for
/// them, and a connection past that is answered busy by the accept loop
/// itself. `handle` runs once a request, and a panic inside it is that
/// request's 500, not the worker's end.
pub fn serve<H>(listener: &TcpListener, workers: usize, queue: usize, handle: H)
where
    H: Fn(&Request) -> Response + Send + Sync + 'static,
{
    let handle = Arc::new(handle);
    let (tx, rx) = sync_channel::<Waiting>(queue.max(1));
    let rx = Arc::new(Mutex::new(rx));
    let slots = workers.max(1);
    let lane = Arc::new(Mutex::new(Lane::new(slots)));
    for n in 0..slots {
        let rx = Arc::clone(&rx);
        let handle = Arc::clone(&handle);
        let lane = Arc::clone(&lane);
        let spawned = std::thread::Builder::new()
            .name(format!("packsetd-worker-{n}"))
            .spawn(move || work(&rx, &lane, handle.as_ref()));
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
                    Err(TrySendError::Full(waiting)) => shed(&waiting.stream),
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
/// A cheap request runs on the worker that read it. A search or a write
/// runs only while a dear slot is free; otherwise it waits on the lane and
/// this worker goes back to the queue, so health is not stuck behind it.
fn work<H>(rx: &Mutex<Receiver<Waiting>>, lane: &Mutex<Lane>, handle: &H)
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
                    &Response::json(code, serde_json::json!({ "error": why }).to_string()),
                );
                continue;
            }
            Ok(request) => request,
        };
        if is_cheap(&request.method, &request.target) {
            reply_ran(&waiting.stream, waiting.accepted, &request, handle);
            continue;
        }
        let held = Held {
            stream: waiting.stream,
            accepted: waiting.accepted,
            request,
        };
        let admitted = {
            let mut lane = lane.lock().unwrap_or_else(|poison| poison.into_inner());
            lane.begin(held)
        };
        let Some(mut held) = admitted else {
            continue;
        };
        loop {
            reply_ran(&held.stream, held.accepted, &held.request, handle);
            let next = {
                let mut lane = lane.lock().unwrap_or_else(|poison| poison.into_inner());
                lane.end()
            };
            match next {
                Some(parked) => held = parked,
                None => break,
            }
        }
    }
}

/// Run `request` unless its client has gone, write the answer and close.
fn reply_ran<H>(stream: &TcpStream, accepted: Instant, request: &Request, handle: &H)
where
    H: Fn(&Request) -> Response,
{
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_nodelay(true);
    // A client that timed out and closed while its request waited already
    // has its error, so the request is not run: a write it gave up on is
    // not applied.
    if abandoned(stream) {
        return;
    }
    let response = run(request, accepted, handle);
    reply(stream, &response);
}

fn reply(stream: &TcpStream, response: &Response) {
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
    let probe = head[..seen].starts_with(b"GET /health ");
    let response = if probe {
        Response::text(200, "packsetd ok, busy")
    } else {
        Response::json(
            503,
            r#"{"error":"packsetd is busy: every worker is answering and the queue is full; try again"}"#
                .to_string(),
        )
    };
    let _ = write_response(stream, &response, &[("Retry-After", "1")]);
    let _ = stream.shutdown(std::net::Shutdown::Write);
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
    response: &Response,
    extra: &[(&str, &str)],
) -> io::Result<()> {
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
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&response.body);
    stream.write_all(&out)?;
    stream.flush()
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
        }
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
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A server on a free port with `workers` and `queue`, answering with
    /// `handle`, in a thread that lives for the test run.
    fn start<H>(workers: usize, queue: usize, handle: H) -> u16
    where
        H: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
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
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
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
    fn dear_slots_leave_one_worker_for_a_pool() {
        assert_eq!(dear_limit(4), 3);
        assert_eq!(dear_limit(1), 1);
        assert!(is_cheap(&Method::Get, "/health"));
        assert!(is_cheap(&Method::Get, "/v1/status?workspace=seat"));
        assert!(is_cheap(&Method::Get, "/v1/workspaces"));
        assert!(is_cheap(&Method::Get, "/__inside_memd/health"));
        assert!(!is_cheap(
            &Method::Get,
            "/v1/search?workspace=seat&q=fusion"
        ));
        assert!(!is_cheap(&Method::Post, "/health"));
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
}
