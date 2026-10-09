//! A small blocking HTTP/1.1 server, on `std::net`.
//!
//! # Why not Axum
//!
//! Tonans originally used Axum. It does not any more, and the reason is worth
//! writing down because "we wrote our own HTTP server" is normally a warning sign.
//!
//! Axum brings Tokio, which brings Mio, which brings `windows-sys`, which brings
//! sixty-odd crates into the dependency tree of a binary whose entire pitch is that
//! you can audit it. All of that machinery exists to serve tens of thousands of
//! concurrent connections. Tonans serves **one user, on loopback, at human speed**:
//! a few dozen requests per interaction, from a single browser tab, on a socket no
//! other machine can reach. None of that concurrency is load-bearing here, and
//! every crate in the tree is something a security review has to account for.
//!
//! So this module is the whole HTTP surface: request parsing, a bounded thread
//! pool, and a response writer. It is about three hundred lines, it has no
//! dependencies, and it is small enough to read in one sitting. That is the point.
//!
//! # What is deliberately not implemented
//!
//! No TLS (loopback), no HTTP/2, no chunked *request* bodies, no compression, no
//! websockets, no file uploads by multipart. If a request needs any of those, it is
//! not a request Tonans should be answering.
//!
//! # Hardening
//!
//! Every one of these exists because the socket is reachable by any process — and
//! any *web page* — on this machine:
//!
//! * A read timeout, so a connection that dribbles bytes cannot hold a worker open.
//! * A bounded request line, a bounded header count, and a bounded header size, all
//!   checked while reading rather than after.
//! * A body limit passed in by the caller, checked against `Content-Length` *before*
//!   a single byte of body is read.
//! * A fixed-size worker pool and bounded accept queue, so connections in flight
//!   cannot grow without limit.
//! * Requests without `Content-Length` are read as having no body. There is no
//!   `Transfer-Encoding: chunked` support, so there is no request smuggling surface.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock;

/// Longest request line (`GET /path HTTP/1.1`) accepted.
const MAX_REQUEST_LINE: usize = 8 * 1024;
/// Most headers accepted.
const MAX_HEADERS: usize = 64;
/// Longest single header line accepted.
const MAX_HEADER_LINE: usize = 8 * 1024;
/// How long a connection may go without sending anything.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Absolute time allowed from the first parse attempt through the request body.
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);
/// How long a response write may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Connections handled at once.
const WORKERS: usize = 8;
/// Accepted connections waiting for a worker.
const QUEUE_CAPACITY: usize = WORKERS;

/// A parsed request.
#[derive(Debug)]
pub struct Request {
    /// `GET`, `POST`, `PUT`.
    pub method: String,
    /// The path, percent-decoded, without the query string.
    pub path: String,
    /// Header names, lowercased.
    pub headers: BTreeMap<String, String>,
    /// The body. Empty unless `Content-Length` said otherwise.
    pub body: Vec<u8>,
}

impl Request {
    /// A header, by lowercase name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The path split on `/`, with empty segments dropped.
    #[must_use]
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
}

/// A response to write.
#[derive(Debug)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A response with a content type and a body.
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body: body.into(),
        }
    }

    /// A JSON response.
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::new(status, "application/json", body)
    }

    /// Add a header.
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// What a handler must be.
pub trait Handler: Send + Sync + 'static {
    /// Reject a request from its method, path, and headers before any body is read.
    /// Security gates belong here so an unauthorized peer cannot make Tonans
    /// allocate or receive a large authenticated-route body first.
    fn preflight(&self, _request: &Request) -> Option<Response> {
        None
    }

    /// Answer a request. A panic is contained to its worker and drops only that
    /// connection, so one bad request cannot take the server down.
    fn handle(&self, request: &Request) -> Response;

    /// The largest body this path may carry, checked before it is read.
    fn body_limit(&self, method: &str, path: &str) -> usize;
}

/// Serve until `stop` is set.
///
/// Blocks. `stop` is polled between accepts, and the caller is expected to poke the
/// listener afterwards (by connecting to it) so a blocked `accept` returns.
pub fn serve<H: Handler>(
    listener: &TcpListener,
    handler: Arc<H>,
    stop: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let (sender, receiver) = mpsc::sync_channel::<TcpStream>(QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    let mut workers = Vec::with_capacity(WORKERS);

    for _ in 0..WORKERS {
        let receiver = Arc::clone(&receiver);
        let handler = Arc::clone(&handler);
        workers.push(std::thread::spawn(move || loop {
            // The lock is held only long enough to take one connection, so workers do
            // not serialise behind each other while handling requests.
            let stream = {
                let Ok(guard) = receiver.lock() else { return };
                guard.recv()
            };
            let Ok(stream) = stream else { return };

            // A panic in one request must not poison the pool or kill the process.
            // The client gets a 500 and the worker carries on.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                connection(stream, handler.as_ref());
            }));
            if outcome.is_err() {
                eprintln!("tonans: a request handler panicked; the connection was dropped");
            }
        }));
    }

    for incoming in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match incoming {
            Ok(stream) => match sender.try_send(stream) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(mut stream)) => {
                    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
                    let response = Response::json(
                        503,
                        r#"{"code":"http.busy","message":"Tonans is busy; retry shortly."}"#,
                    );
                    let _ = write_response(&mut stream, &response);
                    let _ = stream.shutdown(Shutdown::Both);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            },
            Err(error) => {
                eprintln!("tonans: accept failed: {error}");
            }
        }
    }

    drop(sender);
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn connection<H: Handler>(stream: TcpStream, handler: &H) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => DeadlineStream::new(clone, REQUEST_DEADLINE),
        Err(_) => return,
    });
    let mut writer = stream;

    // One request per connection, then close. Keep-alive would save a few
    // milliseconds on a loopback socket and would double the state this module has
    // to reason about. It is not a trade worth making here.
    let response = match read_request_head(&mut reader) {
        Ok(mut request) => match handler.preflight(&request) {
            Some(refusal) => refusal,
            None => {
                let limit = handler.body_limit(&request.method, &request.path);
                match read_body(&mut reader, &request.headers, limit) {
                    Ok(body) => {
                        request.body = body;
                        handler.handle(&request)
                    }
                    Err(status) => bad_request(status),
                }
            }
        },
        Err(status) => bad_request(status),
    };

    let _ = write_response(&mut writer, &response);
    let _ = writer.flush();
    let _ = writer.shutdown(Shutdown::Both);
}

fn bad_request(status: u16) -> Response {
    Response::json(
        status,
        format!(
            r#"{{"code":"http.bad_request","message":"The request was malformed or exceeded a limit. Status {status}."}}"#
        ),
    )
}

struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn new(stream: TcpStream, allowance: Duration) -> Self {
        Self {
            stream,
            deadline: clock::monotonic() + allowance,
        }
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(clock::monotonic());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "absolute request deadline expired",
            ));
        }
        self.stream
            .set_read_timeout(Some(remaining.min(READ_TIMEOUT)))?;
        self.stream.read(buffer).map_err(|error| {
            // SO_RCVTIMEO reports EAGAIN/WouldBlock on Unix and TimedOut on
            // Windows. Expose one portable timeout contract to the parser.
            if error.kind() == std::io::ErrorKind::WouldBlock {
                std::io::Error::new(std::io::ErrorKind::TimedOut, error)
            } else {
                error
            }
        })
    }
}

/// Read one request, or return the status to fail it with.
fn read_request_head(reader: &mut BufReader<DeadlineStream>) -> Result<Request, u16> {
    let mut line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_REQUEST_LINE as u64)
        .read_line(&mut line)
        .map_err(|_| 400u16)?;
    if read == 0 {
        return Err(400);
    }
    if !line.ends_with('\n') {
        // The line filled the cap without terminating.
        return Err(414);
    }

    let mut parts = line.trim_end().split(' ');
    let method = parts.next().ok_or(400u16)?.to_string();
    let target = parts.next().ok_or(400u16)?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    if !version.starts_with("HTTP/1.") {
        return Err(505);
    }
    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "HEAD" | "OPTIONS") {
        return Err(405);
    }

    // The query string is dropped: the launch token arrives there once, on the first
    // navigation, and the page hands it back as a header. Nothing else in the API is
    // addressed by query.
    let path_part = target.split('?').next().unwrap_or("/");
    let path = percent_decode(path_part)?;

    let mut headers = BTreeMap::new();
    for _ in 0..MAX_HEADERS {
        let mut header = String::new();
        let read = reader
            .by_ref()
            .take(MAX_HEADER_LINE as u64)
            .read_line(&mut header)
            .map_err(|_| 400u16)?;
        if read == 0 {
            return Err(400);
        }
        if !header.ends_with('\n') {
            return Err(431);
        }
        let header = header.trim_end();
        if header.is_empty() {
            // Blank line: end of headers. The body is deliberately not read until
            // the handler's preflight security envelope accepts this request.
            return Ok(Request {
                method,
                path,
                headers,
                body: Vec::new(),
            });
        }
        let (name, value) = header.split_once(':').ok_or(400u16)?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Err(431) // too many headers
}

fn read_body(
    reader: &mut BufReader<DeadlineStream>,
    headers: &BTreeMap<String, String>,
    limit: usize,
) -> Result<Vec<u8>, u16> {
    // No `Transfer-Encoding: chunked`. Accepting it would mean two ways to frame a
    // body, which is where request smuggling lives. A client that sends it gets a
    // clean refusal.
    if headers.contains_key("transfer-encoding") {
        return Err(411);
    }
    let Some(raw) = headers.get("content-length") else {
        return Ok(Vec::new());
    };
    let length: usize = raw.parse().map_err(|_| 400u16)?;

    // Checked *before* allocating or reading. A `Content-Length: 4000000000` must
    // cost us nothing.
    if length > limit {
        return Err(413);
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|_| 400u16)?;
    Ok(body)
}

fn write_response(writer: &mut impl Write, response: &Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    };

    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        // A header value containing CR or LF would let a handler inject a second
        // response. Nothing in Tonans builds a header from untrusted input, but this
        // is the line where that would stop being true, so it is checked here.
        if value.contains(['\r', '\n']) || name.contains(['\r', '\n']) {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!("content-length: {}\r\n", response.body.len()));
    head.push_str("connection: close\r\n\r\n");

    writer.write_all(head.as_bytes())?;
    writer.write_all(&response.body)?;
    Ok(())
}

/// Percent-decode a path segment.
fn percent_decode(input: &str) -> Result<String, u16> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(400);
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| 400u16)?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| 400u16)?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    // A path that is not UTF-8 after decoding is not a path we serve.
    String::from_utf8(out).map_err(|_| 400u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AcceptingHandler;

    impl Handler for AcceptingHandler {
        fn handle(&self, _request: &Request) -> Response {
            Response::new(200, "text/plain", "accepted")
        }

        fn body_limit(&self, _method: &str, _path: &str) -> usize {
            usize::MAX
        }
    }

    struct RefusingHandler;

    impl Handler for RefusingHandler {
        fn preflight(&self, _request: &Request) -> Option<Response> {
            Some(Response::new(403, "text/plain", "refused before body"))
        }

        fn handle(&self, _request: &Request) -> Response {
            panic!("a refused request must never reach the body handler")
        }

        fn body_limit(&self, _method: &str, _path: &str) -> usize {
            usize::MAX
        }
    }

    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn percent_decoding_handles_the_names_a_project_can_have() {
        assert_eq!(
            percent_decode("/api/projects/Warehouse%203").unwrap(),
            "/api/projects/Warehouse 3"
        );
        assert_eq!(percent_decode("/plain").unwrap(), "/plain");
        // A decoded traversal is still just a string here; `paths.rs` is what refuses
        // it. But a malformed escape must not panic or silently truncate.
        assert_eq!(percent_decode("/%2e%2e%2f").unwrap(), "/../");
        assert!(percent_decode("/%zz").is_err());
        assert!(percent_decode("/%2").is_err());
    }

    #[test]
    fn a_response_never_lets_a_header_inject_another_response() {
        let mut sink = Vec::new();
        let response = Response::new(200, "text/plain", "hi")
            .with_header("x-safe", "fine")
            .with_header("x-evil", "a\r\nSet-Cookie: stolen=1");

        write_response(&mut sink, &response).unwrap();
        let wire = String::from_utf8(sink).unwrap();
        assert!(wire.contains("x-safe: fine\r\n"));
        assert!(!wire.contains("x-evil"));
        assert!(!wire.contains("Set-Cookie"));
        assert!(wire.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn request_segments_ignore_empty_pieces() {
        let request = Request {
            method: "GET".into(),
            path: "/api/projects/Site A/run".into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        };
        assert_eq!(request.segments(), vec!["api", "projects", "Site A", "run"]);
    }

    #[test]
    fn preflight_refusal_is_written_without_waiting_for_the_claimed_body() {
        let (mut client, server) = socket_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let worker = std::thread::spawn(move || connection(server, &RefusingHandler));
        client
            .write_all(
                b"POST /api/upload HTTP/1.1\r\nHost: wrong\r\nContent-Length: 999999\r\n\r\n",
            )
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap();

        assert!(
            response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{response}"
        );
        assert!(response.ends_with("refused before body"), "{response}");
    }

    #[test]
    fn a_physical_header_line_must_end_inside_its_byte_cap() {
        let (mut client, server) = socket_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let worker = std::thread::spawn(move || connection(server, &AcceptingHandler));
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nx-long: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_LINE)
        );
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap();

        assert!(
            response.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"),
            "{response}"
        );
    }

    #[test]
    fn the_reader_enforces_an_absolute_deadline_across_multiple_reads() {
        let (mut client, server) = socket_pair();
        client.write_all(b"a").unwrap();
        let mut reader = BufReader::new(DeadlineStream::new(server, Duration::from_millis(25)));
        let started = clock::monotonic();
        let mut two_bytes = [0u8; 2];
        let error = reader.read_exact(&mut two_bytes).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
