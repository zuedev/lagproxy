//! Tiny HTTP/1.1 control API, bound to localhost by default.
//!
//! ```text
//! GET  /stats           -> counters as JSON
//! GET  /config          -> current conditions as JSON
//! POST /set  {"latency":"200ms","up:loss":"5%"}
//! ```

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::conditions::Config;
use crate::stats::Stats;

const MAX_BODY: usize = 64 * 1024;
const MAX_HEADERS: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Serves requests forever on an already bound listener.
pub fn serve(listener: TcpListener, config: Arc<RwLock<Config>>, stats: Arc<Stats>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            thread::sleep(Duration::from_millis(10));
            continue;
        };
        let (config, stats) = (config.clone(), stats.clone());
        thread::spawn(move || {
            let _ = handle(stream, &config, &stats);
        });
    }
}

fn handle(mut stream: TcpStream, config: &RwLock<Config>, stats: &Stats) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let (status, reply) = match read_request(&mut stream) {
        Err(e) if e.kind() == io::ErrorKind::InvalidData => (413, json!({ "error": e.to_string() })),
        Err(e) => return Err(e),
        Ok((method, path, body)) => match (method.as_str(), path.as_str()) {
            ("GET", "/stats") => (200, stats.to_json()),
            ("GET", "/config") | ("GET", "/get") => (200, config.read().unwrap().to_json()),
            ("POST", "/set") => match apply(&body, config) {
                Ok(()) => (200, json!({ "ok": true, "config": config.read().unwrap().to_json() })),
                Err(e) => (400, json!({ "ok": false, "error": e })),
            },
            _ => (404, json!({ "error": "not found; try GET /stats, GET /config or POST /set" })),
        },
    };
    let body = reply.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        match status {
            200 => "OK",
            400 => "Bad Request",
            413 => "Payload Too Large",
            _ => "Not Found",
        },
        body.len()
    )
}

/// Applies every key in a JSON object, all or nothing.
pub fn apply(body: &[u8], config: &RwLock<Config>) -> Result<(), String> {
    let map: serde_json::Map<String, Value> =
        serde_json::from_slice(body).map_err(|e| format!("body must be a JSON object: {e}"))?;
    let mut next = config.read().unwrap().clone();
    for (key, value) in map {
        let value = match value {
            Value::String(s) => s,
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            other => return Err(format!("'{key}': expected a string or number, got {other}")),
        };
        next.set(&key, &value)?;
    }
    *config.write().unwrap() = next;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> io::Result<(String, String, Vec<u8>)> {
    // Never read more than one bounded request, however long the client keeps sending.
    let mut reader = BufReader::new(stream.take((MAX_HEADERS + MAX_BODY) as u64));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().split('?').next().unwrap_or_default().to_string();

    let mut content_length = 0;
    let mut header_bytes = line.len();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADERS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too large"));
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    Ok((method, path, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_sets_all_keys_atomically() {
        let config = RwLock::new(Config::default());
        apply(br#"{"latency":"200ms","up:loss":"5%","dup":0.01}"#, &config).unwrap();
        let c = config.read().unwrap();
        assert_eq!(c.up.latency, Duration::from_millis(200));
        assert_eq!(c.up.loss, 0.05);
        assert_eq!(c.down.loss, 0.0);
        assert_eq!(c.down.dup, 0.01);
        drop(c);

        // One bad key means nothing changes.
        assert!(apply(br#"{"latency":"1ms","bogus":"x"}"#, &config).is_err());
        assert_eq!(config.read().unwrap().up.latency, Duration::from_millis(200));
        assert!(apply(b"[1,2]", &config).is_err());
    }
}
