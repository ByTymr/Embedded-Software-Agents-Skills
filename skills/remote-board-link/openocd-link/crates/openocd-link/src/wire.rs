//! Çerçeve biçimi, kanal numaraları ve tanıtım (hello) mesajları.
//!
//! Çerçeve: `u16 BE len` + `u8 kanal` + yük (len − 1 bayt), 1 ≤ len ≤ 60001.

use serde_json::Value;
use std::io::{self, Read, Write};

pub const CONTROL: u8 = 0x00;
pub const FILE: u8 = 0x10;
pub const SHELL: u8 = 0x20;
pub const EXEC_OUT: u8 = 0x30;
pub const PING: u8 = 0xFE;
pub const MAX_PAYLOAD: usize = 60_000;
pub const MAX_HELLO: usize = 4096;
pub const MAX_SERVICES: usize = 8;
pub const PROTO: u64 = 1;

pub const SHELL_DATA: u8 = 1;
pub const SHELL_RESIZE: u8 = 2;
pub const SHELL_EOF: u8 = 3;

/// Tünel `i` (0 tabanlı) için kanal numarası
pub fn tunnel_channel(index: usize) -> u8 {
    1 + index as u8
}

fn known(channel: u8) -> bool {
    matches!(channel, CONTROL | 0x01..=0x08 | FILE | SHELL | EXEC_OUT | PING)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Başlık ve yük tek `write_all` ile gider
pub fn write_frame(w: &mut impl Write, channel: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD || !known(channel) {
        return Err(bad("Geçersiz çerçeve"));
    }
    let mut buf = Vec::with_capacity(payload.len() + 3);
    buf.extend_from_slice(&((payload.len() + 1) as u16).to_be_bytes());
    buf.push(channel);
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 3];
    r.read_exact(&mut head)?;
    let len = u16::from_be_bytes([head[0], head[1]]) as usize;
    if !(1..=MAX_PAYLOAD + 1).contains(&len) || !known(head[2]) {
        return Err(bad("Protokol hatası: geçersiz çerçeve"));
    }
    let mut payload = vec![0u8; len - 1];
    r.read_exact(&mut payload)?;
    Ok((head[2], payload))
}

/// Tanıtım mesajı: `u16 BE uzunluk` + UTF-8 JSON, en fazla 4096 bayt
pub fn write_hello(w: &mut impl Write, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| bad(&e.to_string()))?;
    if bytes.len() > MAX_HELLO {
        return Err(bad("Tanıtım mesajı çok büyük"));
    }
    let mut buf = (bytes.len() as u16).to_be_bytes().to_vec();
    buf.extend(bytes);
    w.write_all(&buf)?;
    w.flush()
}

pub fn read_hello(r: &mut impl Read) -> io::Result<Value> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len)?;
    let len = u16::from_be_bytes(len) as usize;
    if len > MAX_HELLO {
        return Err(bad("Geçersiz tanıtım mesajı"));
    }
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| bad("Geçersiz tanıtım mesajı"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FILE, b"abc").unwrap();
        assert_eq!(buf, [0, 4, 0x10, b'a', b'b', b'c']);
        let (c, p) = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!((c, p.as_slice()), (FILE, &b"abc"[..]));
    }

    #[test]
    fn rejects_unknown_channel_and_bad_length() {
        assert!(read_frame(&mut Cursor::new(vec![0, 1, 0x77])).is_err());
        assert!(read_frame(&mut Cursor::new(vec![0, 0, 0x00])).is_err());
        let too_long = ((MAX_PAYLOAD + 2) as u16).to_be_bytes();
        assert!(read_frame(&mut Cursor::new(vec![too_long[0], too_long[1], 0])).is_err());
        assert!(write_frame(&mut Vec::new(), 0x77, b"").is_err());
        assert!(write_frame(&mut Vec::new(), CONTROL, &vec![0; MAX_PAYLOAD + 1]).is_err());
        assert!(write_frame(&mut Vec::new(), CONTROL, &vec![0; MAX_PAYLOAD]).is_ok());
    }

    #[test]
    fn hello_limits() {
        let mut buf = Vec::new();
        write_hello(&mut buf, &serde_json::json!({"proto": 1})).unwrap();
        assert_eq!(read_hello(&mut Cursor::new(buf)).unwrap()["proto"], 1);
        let big = serde_json::json!({"x": "a".repeat(MAX_HELLO)});
        assert!(write_hello(&mut Vec::new(), &big).is_err());
        assert!(read_hello(&mut Cursor::new(vec![0x7f, 0xff])).is_err());
    }
}
