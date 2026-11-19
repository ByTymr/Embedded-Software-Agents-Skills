//! İmza, parolalı el sıkışma, süre sınırı ve (servis tarafında) deneme sınırı.

use openocd_link_core::net::{self, Secure};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const MAGIC: &[u8; 8] = b"OCDLNK01";

pub enum AuthError {
    /// İmza uymadı: sessizce kapatılır, deneme sayılmaz
    NotOurs,
    Failed(io::Error),
}

/// Okuma başına timeout'a ek olarak bütün aşamayı sınırlar (slowloris dahil).
struct Deadline {
    cancel: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Deadline {
    fn new(socket: &TcpStream, duration: Duration) -> io::Result<Self> {
        let socket = socket.try_clone()?;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            if matches!(rx.recv_timeout(duration), Err(mpsc::RecvTimeoutError::Timeout)) {
                let _ = socket.shutdown(Shutdown::Both);
            }
        });
        Ok(Self {
            cancel: Some(tx),
            worker: Some(worker),
        })
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn authenticate(
    mut socket: TcpStream,
    initiator: bool,
    secret: &str,
) -> Result<Secure, AuthError> {
    let fail = AuthError::Failed;
    socket.set_nodelay(true).map_err(fail)?;
    socket.set_read_timeout(Some(Duration::from_secs(5))).map_err(fail)?;
    socket.set_write_timeout(Some(Duration::from_secs(5))).map_err(fail)?;
    let _deadline = Deadline::new(&socket, Duration::from_secs(12)).map_err(fail)?;
    if initiator {
        socket.write_all(MAGIC).map_err(fail)?;
    } else {
        let mut magic = [0; 8];
        socket.read_exact(&mut magic).map_err(|_| AuthError::NotOurs)?;
        if &magic != MAGIC {
            return Err(AuthError::NotOurs);
        }
    }
    let secure = net::handshake_unlimited(socket, initiator, secret).map_err(fail)?;
    secure.set_read_timeout(Some(Duration::from_secs(5))).map_err(fail)?;
    secure.set_write_timeout(Some(Duration::from_secs(5))).map_err(fail)?;
    Ok(secure)
}

const WINDOW: Duration = Duration::from_secs(60);
const PER_IP: u32 = 5;
const GLOBAL: usize = 30;
const CONCURRENT: usize = 4;

/// Gerçek uzak adres başına ve genel başarısız deneme sınırı (QUIC köprüsünde `peer_addr` hep
/// 127.0.0.1 olduğundan `net.rs` sınırı kullanılamaz).
#[derive(Default)]
pub struct Limiter {
    state: Mutex<LimiterState>,
}

#[derive(Default)]
struct LimiterState {
    per_ip: HashMap<IpAddr, (u32, Instant)>,
    global: VecDeque<Instant>,
    paused_until: Option<Instant>,
    active: usize,
}

pub struct Permit {
    limiter: Arc<Limiter>,
    ip: IpAddr,
    done: bool,
}

impl Limiter {
    /// `None`: bu IP engelli, kabul durdurulmuş ya da 4 el sıkışma sürüyor
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<Permit> {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        if s.paused_until.is_some_and(|t| now < t) || s.active >= CONCURRENT {
            return None;
        }
        if let Some((n, last)) = s.per_ip.get(&ip)
            && *n >= PER_IP
            && now.duration_since(*last) <= WINDOW
        {
            return None;
        }
        s.active += 1;
        Some(Permit {
            limiter: self.clone(),
            ip,
            done: false,
        })
    }
}

impl Permit {
    /// `ok = false`: başarısız deneme olarak sayılır
    pub fn finish(mut self, ok: bool) -> bool {
        self.done = true;
        let mut s = self.limiter.state.lock().unwrap();
        s.active -= 1;
        let now = Instant::now();
        if ok {
            s.per_ip.remove(&self.ip);
            return false;
        }
        let entry = s.per_ip.entry(self.ip).or_insert((0, now));
        if now.duration_since(entry.1) > WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.1 = now;
        s.global.push_back(now);
        while s.global.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
            s.global.pop_front();
        }
        if s.global.len() >= GLOBAL {
            s.paused_until = Some(now + WINDOW);
            s.global.clear();
            return true;
        }
        false
    }

    /// İmza uymadı: deneme sayılmaz
    pub fn ignore(mut self) {
        self.done = true;
        self.limiter.state.lock().unwrap().active -= 1;
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.done {
            self.limiter.state.lock().unwrap().active -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn five_failures_block_that_ip_only() {
        let limiter = Arc::new(Limiter::default());
        for _ in 0..5 {
            limiter.admit(ip(1)).unwrap().finish(false);
        }
        assert!(limiter.admit(ip(1)).is_none());
        assert!(limiter.admit(ip(2)).is_some());
    }

    #[test]
    fn success_clears_the_counter() {
        let limiter = Arc::new(Limiter::default());
        for _ in 0..4 {
            limiter.admit(ip(1)).unwrap().finish(false);
        }
        limiter.admit(ip(1)).unwrap().finish(true);
        for _ in 0..4 {
            limiter.admit(ip(1)).unwrap().finish(false);
        }
        assert!(limiter.admit(ip(1)).is_some());
    }

    #[test]
    fn at_most_four_handshakes_and_ignored_ones_do_not_count() {
        let limiter = Arc::new(Limiter::default());
        let held: Vec<_> = (0..4).map(|_| limiter.admit(ip(1)).unwrap()).collect();
        assert!(limiter.admit(ip(2)).is_none());
        for p in held {
            p.ignore();
        }
        for _ in 0..10 {
            limiter.admit(ip(1)).unwrap().ignore();
        }
        assert!(limiter.admit(ip(1)).is_some());
    }

    #[test]
    fn thirty_failures_pause_everything() {
        let limiter = Arc::new(Limiter::default());
        let mut paused = false;
        for n in 0..30u8 {
            paused = limiter.admit(ip(n)).unwrap().finish(false);
        }
        assert!(paused);
        assert!(limiter.admit(ip(200)).is_none());
    }
}
