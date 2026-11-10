//! mcp-sse-bridge <http://host:port/sse>: a stdio MCP server that forwards to
//! an MCP server on the legacy HTTP+SSE transport. synaps 0.11.1 only spawns
//! stdio servers; Terminal-Bench tasks may provide SSE ones (e.g. Playwright).
//!
//! GET <url> (text/event-stream): the `endpoint` event gives the POST URL;
//! every `message` event is one JSON-RPC message -> one stdout line. Each
//! stdin line is one JSON-RPC message -> POST to the endpoint. Plain HTTP only,
//! std only, static (musl) so it runs in any task container.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;

fn die(msg: &str) -> ! {
    eprintln!("mcp-sse-bridge: {msg}");
    std::process::exit(1)
}

/// (host:port, path) of an http:// URL.
fn split_url(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").unwrap_or_else(|| die(&format!("only http:// URLs: {url}")));
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let auth = if auth.contains(':') { auth.to_string() } else { format!("{auth}:80") };
    (auth, path.to_string())
}

/// Read the status line and headers; returns (status, chunked).
fn read_head(r: &mut impl BufRead) -> (u16, bool) {
    let mut line = String::new();
    r.read_line(&mut line).unwrap_or_else(|e| die(&format!("read status: {e}")));
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let l = line.to_ascii_lowercase();
        if l.starts_with("transfer-encoding:") && l.contains("chunked") {
            chunked = true;
        }
    }
    (status, chunked)
}

/// De-chunks an HTTP/1.1 chunked body into a plain byte stream.
struct Dechunk<R> {
    inner: R,
    left: usize,
    done: bool,
}

impl<R: BufRead> Read for Dechunk<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut size = String::new();
            loop {
                size.clear();
                if self.inner.read_line(&mut size)? == 0 {
                    self.done = true;
                    return Ok(0);
                }
                if !size.trim().is_empty() {
                    break; // skip the CRLF that ends the previous chunk
                }
            }
            let hex = size.trim().split(';').next().unwrap_or("0");
            self.left = usize::from_str_radix(hex, 16).unwrap_or(0);
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let n = buf.len().min(self.left);
        let got = self.inner.read(&mut buf[..n])?;
        self.left -= got;
        Ok(got)
    }
}

fn sse_reader(addr: String, path: String, endpoint_tx: mpsc::Sender<String>) {
    let mut s = TcpStream::connect(&addr).unwrap_or_else(|e| die(&format!("connect {addr}: {e}")));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap_or_else(|e| die(&format!("send GET: {e}")));
    let mut r = BufReader::new(s);
    let (status, chunked) = read_head(&mut r);
    if status != 200 {
        die(&format!("GET {path}: HTTP {status}"));
    }
    let body: Box<dyn Read> = if chunked { Box::new(Dechunk { inner: r, left: 0, done: false }) } else { Box::new(r) };
    let body = BufReader::new(body);
    let (mut event, mut data) = (String::new(), String::new());
    let mut out = std::io::stdout();
    for line in body.lines() {
        let Ok(line) = line else { break };
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            // Dispatch. JSON split over data lines only breaks at whitespace,
            // so joining them without a separator keeps it valid, one line.
            if !data.is_empty() {
                if event == "endpoint" {
                    let _ = endpoint_tx.send(data.clone());
                } else if event.is_empty() || event == "message" {
                    let _ = out.write_all(data.as_bytes()).and_then(|_| out.write_all(b"\n")).and_then(|_| out.flush());
                }
            }
            event.clear();
            data.clear();
        } else if let Some(v) = line.strip_prefix("event:") {
            event = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push_str(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    die("SSE stream closed");
}

fn post(addr: &str, path: &str, body: &str) -> Result<u16, String> {
    let mut s = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| format!("send POST: {e}"))?;
    let mut r = BufReader::new(s);
    let (status, _) = read_head(&mut r);
    let mut sink = Vec::new();
    let _ = r.read_to_end(&mut sink);
    Ok(status)
}

fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| die("usage: mcp-sse-bridge <http://host:port/sse>"));
    let (addr, path) = split_url(&url);
    let (tx, rx) = mpsc::channel();
    {
        let (addr, path) = (addr.clone(), path.clone());
        std::thread::spawn(move || sse_reader(addr, path, tx));
    }
    let endpoint = rx.recv().unwrap_or_else(|_| die("no endpoint event"));
    let (post_addr, post_path) = if endpoint.starts_with("http://") { split_url(&endpoint) } else { (addr, endpoint) };
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match post(&post_addr, &post_path, &line) {
            Ok(s) if (200..300).contains(&s) => {}
            Ok(s) => eprintln!("mcp-sse-bridge: POST {post_path}: HTTP {s}"),
            Err(e) => eprintln!("mcp-sse-bridge: {e}"),
        }
    }
}
