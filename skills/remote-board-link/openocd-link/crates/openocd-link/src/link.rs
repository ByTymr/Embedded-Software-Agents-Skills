//! Kimliği doğrulanmış şifreli akış üzerindeki oturum: tek yazıcı, ayrı okuyucu, PING.
//!
//! Tek yazıcı kuralı: iki yazıcı aynı Noise nonce'unu kullanır ve oturum bozulur. Okuyucu
//! `try_clone()` kopyasıdır ve asla yazmaz. Kopyalar hello değişiminden SONRA alınmalıdır.

use crate::wire;
use openocd_link_core::net::Secure;
use serde_json::Value;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const READ_TIMEOUT: Duration = Duration::from_secs(15);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PING_EVERY: Duration = Duration::from_secs(3);

pub struct Link {
    writer: Mutex<Secure>,
    closer: Secure,
    pub alive: AtomicBool,
    pub sent: AtomicU64,
    pub received: AtomicU64,
}

impl Link {
    pub fn new(secure: Secure) -> io::Result<(Arc<Self>, Secure)> {
        secure.set_read_timeout(Some(READ_TIMEOUT))?;
        secure.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let reader = secure.try_clone()?;
        let closer = secure.try_clone()?;
        let link = Arc::new(Self {
            writer: Mutex::new(secure),
            closer,
            alive: AtomicBool::new(true),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
        });
        let ping = link.clone();
        thread::spawn(move || {
            while ping.is_alive() {
                thread::sleep(PING_EVERY);
                if ping.send(wire::PING, &[]).is_err() {
                    break;
                }
            }
        });
        Ok((link, reader))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Her çerçeve ayrı kilitle gider; böylece tünel trafiği dosya aktarımı sırasında da akar
    pub fn send(&self, channel: u8, payload: &[u8]) -> io::Result<()> {
        if !self.is_alive() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "Oturum kapalı"));
        }
        let result = wire::write_frame(&mut *self.writer.lock().unwrap(), channel, payload);
        match result {
            Ok(()) => {
                self.sent.fetch_add(payload.len() as u64 + 3, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.close();
                Err(e)
            }
        }
    }

    pub fn send_json(&self, value: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        self.send(wire::CONTROL, &bytes)
    }

    pub fn close(&self) {
        self.alive.store(false, Ordering::Release);
        self.closer.shutdown();
    }
}

/// Çerçeveleri `handler`'a verir. Bittiğinde nedenini Türkçe metin olarak döner. PING'ler
/// burada tüketilir.
pub fn read_loop(
    link: &Arc<Link>,
    mut reader: Secure,
    mut handler: impl FnMut(u8, Vec<u8>) -> io::Result<()>,
) -> String {
    let reason = loop {
        match wire::read_frame(&mut reader) {
            Ok((channel, payload)) => {
                link.received.fetch_add(payload.len() as u64 + 3, Ordering::Relaxed);
                if channel == wire::PING {
                    continue;
                }
                if let Err(e) = handler(channel, payload) {
                    break format!("Oturum hatası: {e}");
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                break "Bağlantı koptu (15 sn yanıt yok)".into();
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                break format!("Protokol hatası: {e}");
            }
            Err(_) if !link.is_alive() => break "Oturum kapatıldı".into(),
            Err(_) => break "Bağlantı koptu".into(),
        }
    };
    link.close();
    reason
}
