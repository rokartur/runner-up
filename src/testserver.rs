//! A local HTTP/1.1 server for tests, as Go's httptest.Server: each
//! request goes to the handler on its own thread and the connection closes
//! after the answer.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

pub struct Request {
    pub method: String,
    /// The path without the query.
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> &str {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map_or("", |(_, v)| v)
    }

    pub fn query_value(&self, key: &str) -> Option<&str> {
        self.query.split('&').find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response { status, headers: vec![], body: body.into() }
    }

    pub fn json(status: u16, v: &serde_json::Value) -> Response {
        let mut r = Response::new(status, v.to_string());
        r.headers.push(("Content-Type".into(), "application/json".into()));
        r
    }
}

pub struct Server {
    pub url: String,
}

impl Server {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(conn.try_clone().unwrap());
                    let Some(req) = read_request(&mut reader) else { return };
                    let resp = handler(&req);
                    let mut out = conn;
                    let mut head = format!("HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, reason(resp.status), resp.body.len());
                    for (k, v) in &resp.headers {
                        head += &format!("{k}: {v}\r\n");
                    }
                    head += "\r\n";
                    let _ = out.write_all(head.as_bytes());
                    let _ = out.write_all(&resp.body);
                });
            }
        });
        Server { url }
    }
}

fn read_request(r: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = vec![];
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request { method, path: path.into(), query: query.into(), headers, body })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
