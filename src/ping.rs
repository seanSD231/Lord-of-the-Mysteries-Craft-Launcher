//! Asking the server how it is doing, with the same handshake the multiplayer list uses.
//!
//! Server List Ping: connect, send a handshake with `next state = status`, send an empty
//! status request, read back a JSON blob with the MOTD, the version and the player count.
//! No account and no mods required, which is why the launcher can show it before anyone
//! has signed in.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// 1.21.1. The server answers a status request whatever this says, so it only has to be
/// plausible.
const PROTOCOL: i32 = 767;
const TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Serialize, Default)]
pub struct ServerStatus {
    pub online: bool,
    pub players: u32,
    pub max_players: u32,
    pub version: String,
    pub motd: String,
    pub ping_ms: u64,
    /// Why it could not be reached, for the one line the window shows.
    pub error: String,
}

fn write_varint(out: &mut Vec<u8>, mut v: i32) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v = ((v as u32) >> 7) as i32;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn read_varint(r: &mut impl Read) -> Result<i32> {
    let mut result: i32 = 0;
    for shift in 0..5 {
        let mut b = [0u8; 1];
        r.read_exact(&mut b).context("reading a varint")?;
        result |= ((b[0] & 0x7f) as i32) << (shift * 7);
        if b[0] & 0x80 == 0 {
            return Ok(result);
        }
    }
    bail!("varint is longer than five bytes")
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as i32);
    out.extend_from_slice(s.as_bytes());
}

/// Wraps a payload in its length prefix, which is how every packet goes out.
fn framed(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    write_varint(&mut out, payload.len() as i32);
    out.extend_from_slice(payload);
    out
}

pub fn status(address: &str) -> ServerStatus {
    match query(address) {
        Ok(s) => s,
        Err(e) => ServerStatus {
            online: false,
            error: e.to_string(),
            ..Default::default()
        },
    }
}

fn query(address: &str) -> Result<ServerStatus> {
    let (host, port) = match address.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(25565)),
        None => (address.to_string(), 25565),
    };

    let started = Instant::now();
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("looking up {host}"))?
        .next()
        .ok_or_else(|| anyhow!("{host} did not resolve"))?;
    let mut sock = TcpStream::connect_timeout(&addr, TIMEOUT)
        .with_context(|| format!("connecting to {address}"))?;
    sock.set_read_timeout(Some(TIMEOUT))?;
    sock.set_write_timeout(Some(TIMEOUT))?;

    let mut handshake = Vec::new();
    write_varint(&mut handshake, 0x00);
    write_varint(&mut handshake, PROTOCOL);
    write_string(&mut handshake, &host);
    handshake.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut handshake, 1); // next state: status
    sock.write_all(&framed(&handshake))?;
    sock.write_all(&framed(&[0x00]))?; // status request
    sock.flush()?;

    let len = read_varint(&mut sock)? as usize;
    if len == 0 || len > 4 << 20 {
        bail!("the server sent a status packet of {len} bytes");
    }
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body)
        .context("reading the status response")?;

    let mut cursor = &body[..];
    let packet_id = read_varint(&mut cursor)?;
    if packet_id != 0x00 {
        bail!("expected a status response, got packet {packet_id}");
    }
    let json_len = read_varint(&mut cursor)? as usize;
    if json_len > cursor.len() {
        bail!("the status response was truncated");
    }
    let json: serde_json::Value = serde_json::from_slice(&cursor[..json_len])
        .context("parsing the status response")?;

    Ok(ServerStatus {
        online: true,
        players: json["players"]["online"].as_u64().unwrap_or(0) as u32,
        max_players: json["players"]["max"].as_u64().unwrap_or(0) as u32,
        version: json["version"]["name"].as_str().unwrap_or("").to_string(),
        motd: flatten_motd(&json["description"]),
        ping_ms: started.elapsed().as_millis() as u64,
        error: String::new(),
    })
}

/// A MOTD can be a plain string, or a tree of components with `text` and `extra`.
/// Everything is flattened to one line, and colour codes are dropped.
fn flatten_motd(v: &serde_json::Value) -> String {
    let mut out = String::new();
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => out.push_str(s),
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            serde_json::Value::Object(o) => {
                if let Some(serde_json::Value::String(t)) = o.get("text") {
                    out.push_str(t);
                }
                if let Some(e) = o.get("extra") {
                    walk(e, out);
                }
            }
            _ => {}
        }
    }
    walk(v, &mut out);
    // strip the section-sign colour codes servers put in plain MOTDs
    let mut clean = String::with_capacity(out.len());
    let mut chars = out.chars();
    while let Some(c) = chars.next() {
        if c == '\u{a7}' {
            chars.next();
        } else if c == '\n' {
            clean.push(' ');
        } else {
            clean.push(c);
        }
    }
    clean.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 255, 2097151, 767, i32::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut slice = &buf[..];
            assert_eq!(read_varint(&mut slice).unwrap(), v, "value {v}");
        }
    }

    #[test]
    fn motd_flattens_and_strips_colours() {
        let plain = serde_json::json!("\u{a7}6Lord of the \u{a7}eMysteries");
        assert_eq!(flatten_motd(&plain), "Lord of the Mysteries");

        let tree = serde_json::json!({
            "text": "Astral",
            "extra": [{ "text": " World" }, { "text": "!" }]
        });
        assert_eq!(flatten_motd(&tree), "Astral World!");

        let multiline = serde_json::json!("one\ntwo");
        assert_eq!(flatten_motd(&multiline), "one two");
    }

    #[test]
    fn framing_prefixes_the_length() {
        assert_eq!(framed(&[0x00]), vec![1, 0]);
        let big = vec![7u8; 300];
        let f = framed(&big);
        assert_eq!(&f[..2], &[0xac, 0x02]); // 300 as a varint
        assert_eq!(f.len(), 302);
    }
}
