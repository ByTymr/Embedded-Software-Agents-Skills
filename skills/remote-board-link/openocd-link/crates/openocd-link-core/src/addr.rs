//! Adres metni yardımcıları.

use std::io;
use std::net::SocketAddr;

pub fn candidates_text(list: &[SocketAddr]) -> String {
    list.iter()
        .map(SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn parse_list(text: &str) -> io::Result<Vec<SocketAddr>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "Geçersiz bağlantı kodu.");
    let list: Vec<SocketAddr> = text
        .split(',')
        .map(|a| a.trim().parse::<SocketAddr>())
        .collect::<Result<_, _>>()
        .map_err(|_| invalid())?;
    if list.len() > 8
        || list
            .iter()
            .any(|a| a.port() == 0 || a.ip().is_unspecified() || a.ip().is_multicast())
    {
        return Err(invalid());
    }
    Ok(list)
}

/// "[::ffff:1.2.3.4]:5" yerine "1.2.3.4:5"
pub fn show_addr(addr: SocketAddr) -> String {
    SocketAddr::new(addr.ip().to_canonical(), addr.port()).to_string()
}
