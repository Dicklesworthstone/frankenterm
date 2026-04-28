//! Hermetic local HTTP server fixture for no-mocks webhook integration
//! tests (ft-geg03).
//!
//! Why this exists: `WebhookTransport` is a pub trait with no concrete
//! real-HTTP implementer in `frankenterm-core/src/`. The labruntime
//! tests use a `MockTransport` that pretends every URL responds the
//! same way. To audit the dispatcher's behavior against actual HTTP/1.1
//! framing, header propagation, status-code parsing, and
//! connection-failure paths, this fixture spins up a TCP listener on
//! `127.0.0.1:0` (OS-assigned port) inside the test process and serves
//! a single canned response per accepted connection.
//!
//! Standard library only (no httptest / wiremock / reqwest deps). The
//! protocol surface exercised is minimal: POST with Content-Length body,
//! reply with `HTTP/1.1 <status> <reason>\r\nContent-Length: 0\r\n\r\n`.
//! That covers the assertions a no-mocks webhook test cares about.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Single accepted request captured by the local server.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// What the server replies on the next accept.
#[derive(Debug, Clone)]
pub enum CannedResponse {
    Status(u16, &'static str),
    /// Drop the connection without sending a response (simulates a peer
    /// reset / abortive close — surfaces real `WebhookTransport` error
    /// handling rather than a clean status-code path).
    DropConnection,
    /// Hold the connection open without responding for `delay`. Useful
    /// for timeout regression tests if the dispatcher grows one.
    Stall {
        delay: Duration,
        then: Box<CannedResponse>,
    },
}

/// Hermetic HTTP server bound to 127.0.0.1:0. Accepts ONE request per
/// `expect_request` call and replies with the queued response.
pub struct LocalHttpServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Arc<Mutex<Vec<CannedResponse>>>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl LocalHttpServer {
    /// Start the server with one or more queued canned responses (FIFO).
    /// Returns immediately once the listener is bound.
    pub fn start_with_responses(responses: Vec<CannedResponse>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(false)?;

        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(responses));
        let shutdown = Arc::new(AtomicBool::new(false));

        let requests_thread = Arc::clone(&requests);
        let responses_thread = Arc::clone(&responses);
        let shutdown_thread = Arc::clone(&shutdown);
        let accept_thread = thread::spawn(move || {
            // Set a short read timeout so accept() returns periodically and we
            // can observe the shutdown flag without separate signaling.
            listener
                .set_nonblocking(true)
                .expect("set_nonblocking on listener");
            while !shutdown_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        stream.set_nonblocking(false).ok();
                        let _ = handle_connection(
                            stream,
                            Arc::clone(&requests_thread),
                            Arc::clone(&responses_thread),
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            addr,
            requests,
            responses,
            shutdown,
            accept_thread: Some(accept_thread),
        })
    }

    /// `http://127.0.0.1:<port>/`
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `http://127.0.0.1:<port>/<path>`
    pub fn url_path(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Snapshot of all captured requests so far.
    pub fn captured(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// `addr:port` form for tests that need just the host.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for LocalHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept_thread.take() {
            // Give the accept loop one tick to observe the shutdown flag.
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    responses: Arc<Mutex<Vec<CannedResponse>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let (method, path) = if parts.len() >= 2 {
        (parts[0].to_string(), parts[1].to_string())
    } else {
        (String::new(), String::new())
    };

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().to_string();
            if key.eq_ignore_ascii_case("Content-Length") {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((key, val));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    requests.lock().unwrap().push(CapturedRequest {
        method,
        path,
        headers,
        body,
    });

    // Pop the next canned response. If queue is empty, default to 200 OK.
    let response = {
        let mut q = responses.lock().unwrap();
        if q.is_empty() {
            CannedResponse::Status(200, "OK")
        } else {
            q.remove(0)
        }
    };

    write_response(&mut stream, response)?;
    Ok(())
}

fn write_response(stream: &mut TcpStream, response: CannedResponse) -> std::io::Result<()> {
    match response {
        CannedResponse::Status(code, reason) => {
            let payload = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(payload.as_bytes())?;
            stream.flush()?;
        }
        CannedResponse::DropConnection => {
            // Reset the connection without writing.
            stream.shutdown(std::net::Shutdown::Both).ok();
        }
        CannedResponse::Stall { delay, then } => {
            thread::sleep(delay);
            write_response(stream, *then)?;
        }
    }
    Ok(())
}
