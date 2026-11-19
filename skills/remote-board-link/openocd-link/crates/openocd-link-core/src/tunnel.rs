//! Oturum içindeki tek TCP yönlendirmesi. Yerel uç yalnız loopback'te dinler;
//! hedef yalnız paylaşanın açıkça seçtiği loopback portudur. LAN USB servisi değişmez.
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const CHUNK: usize = 8192;
const MAX_CONNECTIONS: usize = 16;
const MAX_PENDING: usize = CHUNK * 8;
pub const MAX_PACKET: usize = CHUNK + 5;
const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;
const READY: u8 = 4;
const FIN: u8 = 5;
const REQUEST: u8 = 6;
const GRANT: u8 = 7;
const DENY: u8 = 8;

pub struct Tunnel {
    /// Paylaşan cihazın izin verdiği servis. Sıfır: kapalı.
    pub allowed_port: AtomicU16,
    /// İzleyenin yerel dinleme portu. Sıfır: kapalı.
    pub local_port: AtomicU16,
    pub remote_port: AtomicU16,
    pub status: Mutex<String>,
    pub pending_port: AtomicU16,
    pub peer_allowed: AtomicU16,
    pub peer_rejected: AtomicU16,
    request: AtomicU16,
    deny: AtomicU16,
    closed: AtomicBool,
    tx: mpsc::SyncSender<Vec<u8>>,
    rx: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
}

fn packet(kind: u8, id: u32, data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![kind];
    bytes.extend(id.to_be_bytes());
    bytes.extend(data);
    bytes
}

fn validate(bytes: &[u8]) -> io::Result<()> {
    let valid = match bytes.first() {
        Some(&OPEN) | Some(&REQUEST) | Some(&GRANT) | Some(&DENY) => bytes.len() == 7,
        Some(&DATA) => (6..=MAX_PACKET).contains(&bytes.len()),
        Some(&CLOSE) | Some(&READY) | Some(&FIN) => bytes.len() == 5,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::other("Geçersiz tünel mesajı"))
    }
}

struct Pipe {
    socket: TcpStream,
    port: u16,
    ready: bool,
    read_finished: bool,
    write_finished: bool,
    pending: Vec<u8>,
    offset: usize,
}
impl Pipe {
    fn new(socket: TcpStream, port: u16, ready: bool) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            port,
            ready,
            read_finished: false,
            write_finished: false,
            pending: Vec::new(),
            offset: 0,
        })
    }
}
impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

impl Tunnel {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::sync_channel(32);
        Arc::new(Self {
            allowed_port: AtomicU16::new(0),
            local_port: AtomicU16::new(0),
            remote_port: AtomicU16::new(0),
            status: Mutex::new("Kapalı".into()),
            pending_port: AtomicU16::new(0),
            peer_allowed: AtomicU16::new(0),
            peer_rejected: AtomicU16::new(0),
            request: AtomicU16::new(0),
            deny: AtomicU16::new(0),
            closed: AtomicBool::new(false),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }

    pub fn request_access(&self, port: u16) {
        if port != 0 {
            self.peer_rejected.store(0, Ordering::Release);
            self.request.store(port, Ordering::Release);
        }
    }

    pub fn answer_access(&self, allow: bool) {
        let port = self.pending_port.swap(0, Ordering::AcqRel);
        if port == 0 {
            return;
        }
        if allow {
            self.allowed_port.store(port, Ordering::Release);
        } else {
            self.deny.store(port, Ordering::Release);
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn receive(&self, bytes: &[u8]) -> io::Result<()> {
        validate(bytes)?;
        self.tx
            .try_send(bytes.to_vec())
            .map_err(|_| io::Error::other("Tünel alım kuyruğu dolu veya kapalı"))
    }

    pub fn run(
        self: &Arc<Self>,
        host: bool,
        alive: impl Fn() -> bool + Send + 'static,
        send: impl Fn(&[u8]) -> io::Result<()> + Send + 'static,
    ) {
        let Some(rx) = self.rx.lock().unwrap().take() else {
            return;
        };
        let this = self.clone();
        thread::spawn(move || {
            let result = this.pump(host, &rx, alive, send);
            *this.status.lock().unwrap() = match result {
                Ok(()) => "Kapalı".into(),
                Err(e) => format!("Tünel durdu: {e}"),
            };
        });
    }

    fn pump(
        &self,
        host: bool,
        rx: &mpsc::Receiver<Vec<u8>>,
        alive: impl Fn() -> bool,
        send: impl Fn(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut listener = None;
        let mut bound = 0;
        let mut next_id = 0u32;
        let mut announced = 0;
        let mut pipes = HashMap::<u32, Pipe>::new();
        let mut buffer = [0; CHUNK];
        let mut turn = 0usize;
        while alive() && !self.closed.load(Ordering::Acquire) {
            let requested = self.local_port.load(Ordering::Acquire);
            let allowed = self.allowed_port.load(Ordering::Acquire);
            if !host && requested != bound {
                listener = None;
                for (id, _) in pipes.drain() {
                    send(&packet(CLOSE, id, &[]))?;
                }
                bound = requested;
                if bound != 0 {
                    match TcpListener::bind(("127.0.0.1", bound)) {
                        Ok(l) => {
                            l.set_nonblocking(true)?;
                            listener = Some(l);
                            *self.status.lock().unwrap() = format!("127.0.0.1:{bound} dinleniyor");
                        }
                        Err(e) => {
                            *self.status.lock().unwrap() =
                                format!("Yerel port açılamadı: {e}. Başka bir yerel port seçin.");
                            self.local_port.store(0, Ordering::Release);
                        }
                    }
                }
            }
            if host && announced != allowed {
                send(&packet(GRANT, 0, &allowed.to_be_bytes()))?;
                announced = allowed;
            }
            let denied = self.deny.swap(0, Ordering::AcqRel);
            if host && denied != 0 {
                send(&packet(DENY, 0, &denied.to_be_bytes()))?;
            }
            let request = self.request.swap(0, Ordering::AcqRel);
            if !host && request != 0 {
                send(&packet(REQUEST, 0, &request.to_be_bytes()))?;
            }
            let revoked: Vec<_> = pipes
                .iter()
                .filter(|(_, p)| {
                    if host {
                        allowed == 0 || allowed != p.port
                    } else {
                        bound == 0 || p.port != self.remote_port.load(Ordering::Acquire)
                    }
                })
                .map(|(&id, _)| id)
                .collect();
            for id in revoked {
                pipes.remove(&id);
                send(&packet(CLOSE, id, &[]))?;
            }
            if !host
                && pipes.len() < MAX_CONNECTIONS
                && let Some(l) = &listener
            {
                match l.accept() {
                    Ok((socket, _)) => {
                        next_id = next_id.wrapping_add(1).max(1);
                        let port = self.remote_port.load(Ordering::Acquire);
                        pipes.insert(next_id, Pipe::new(socket, port, false)?);
                        send(&packet(OPEN, next_id, &port.to_be_bytes()))?;
                        *self.status.lock().unwrap() = format!(
                            "Yerel :{bound} → karşı cihaz :{port} · {} bağlantı",
                            pipes.len()
                        );
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            // Her turda sınırlı paket; kontrol ile farklı bağlantılar birbirini aç bırakmaz.
            for _ in 0..8 {
                let Ok(bytes) = rx.try_recv() else {
                    break;
                };
                let id = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
                match bytes[0] {
                    REQUEST if host => {
                        let port = u16::from_be_bytes([bytes[5], bytes[6]]);
                        if port != 0 {
                            if port == allowed {
                                send(&packet(GRANT, 0, &port.to_be_bytes()))?;
                            } else {
                                self.pending_port.store(port, Ordering::Release);
                            }
                        }
                    }
                    GRANT if !host => {
                        let port = u16::from_be_bytes([bytes[5], bytes[6]]);
                        self.peer_allowed.store(port, Ordering::Release);
                        self.peer_rejected.store(0, Ordering::Release);
                        if port == 0 {
                            self.local_port.store(0, Ordering::Release);
                            *self.status.lock().unwrap() =
                                "Karşı cihaz servis iznini kaldırdı; yönlendirme kapatıldı.".into();
                        }
                    }
                    DENY if !host => self
                        .peer_rejected
                        .store(u16::from_be_bytes([bytes[5], bytes[6]]), Ordering::Release),
                    OPEN if host => {
                        let target = u16::from_be_bytes([bytes[5], bytes[6]]);
                        if id == 0
                            || pipes.contains_key(&id)
                            || pipes.len() >= MAX_CONNECTIONS
                            || target == 0
                            || target != allowed
                        {
                            send(&packet(CLOSE, id, &[]))?;
                        } else {
                            let address = SocketAddr::from(([127, 0, 0, 1], target));
                            match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
                                Ok(socket)
                                    if self.allowed_port.load(Ordering::Acquire) == target =>
                                {
                                    pipes.insert(id, Pipe::new(socket, target, true)?);
                                    send(&packet(READY, id, &[]))?;
                                    *self.status.lock().unwrap() = format!(
                                        "127.0.0.1:{target} paylaşılıyor · {} bağlantı",
                                        pipes.len()
                                    );
                                }
                                _ => send(&packet(CLOSE, id, &[]))?,
                            }
                        }
                    }
                    READY if !host => {
                        if let Some(p) = pipes.get_mut(&id) {
                            p.ready = true;
                        }
                    }
                    DATA => {
                        if let Some(p) = pipes.get_mut(&id)
                            && p.ready
                        {
                            if p.pending.len() + bytes.len() - 5 > MAX_PENDING {
                                pipes.remove(&id);
                                send(&packet(CLOSE, id, &[]))?;
                            } else {
                                p.pending.extend_from_slice(&bytes[5..]);
                            }
                        }
                    }
                    FIN => {
                        if let Some(p) = pipes.get_mut(&id) {
                            p.write_finished = true;
                        }
                    }
                    CLOSE => {
                        pipes.remove(&id);
                    }
                    _ => {}
                }
            }
            let mut ended = Vec::new();
            for (&id, p) in &mut pipes {
                if p.offset < p.pending.len() {
                    match p.socket.write(&p.pending[p.offset..]) {
                        Ok(0) => {
                            ended.push(id);
                            continue;
                        }
                        Ok(n) => {
                            p.offset += n;
                            if p.offset == p.pending.len() {
                                p.pending.clear();
                                p.offset = 0;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            ended.push(id);
                            continue;
                        }
                    }
                }
                if p.write_finished && p.pending.is_empty() {
                    let _ = p.socket.shutdown(Shutdown::Write);
                }
                if p.read_finished && p.write_finished && p.pending.is_empty() {
                    ended.push(id);
                }
            }
            for id in ended {
                pipes.remove(&id);
                send(&packet(CLOSE, id, &[]))?;
            }
            // Ortak gidiş kuyruğuna tur başına bir veri parçası; giriş olayları önceliğini korur.
            let ids: Vec<_> = pipes.keys().copied().collect();
            if !ids.is_empty() {
                turn = turn.wrapping_add(1);
                for i in 0..ids.len() {
                    let id = ids[(turn + i) % ids.len()];
                    let p = pipes.get_mut(&id).unwrap();
                    if !p.ready || p.read_finished {
                        continue;
                    }
                    match p.socket.read(&mut buffer) {
                        Ok(0) => {
                            p.read_finished = true;
                            send(&packet(FIN, id, &[]))?;
                        }
                        Ok(n) => {
                            send(&packet(DATA, id, &buffer[..n]))?;
                            break;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            pipes.remove(&id);
                            send(&packet(CLOSE, id, &[]))?;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait(mut test: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !test() {
            assert!(Instant::now() < deadline, "tünel zaman aşımı");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn pair() -> (Arc<Tunnel>, Arc<Tunnel>) {
        let host = Tunnel::new();
        let client = Tunnel::new();
        let h = host.clone();
        client.run(false, || true, move |bytes| h.receive(bytes));
        let c = client.clone();
        host.run(true, || true, move |bytes| c.receive(bytes));
        (host, client)
    }

    fn connect(client: &Tunnel, target: u16) -> TcpStream {
        let reserve = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        client.remote_port.store(target, Ordering::Release);
        client.local_port.store(port, Ordering::Release);
        let mut socket = None;
        wait(|| {
            socket = TcpStream::connect(("127.0.0.1", port)).ok();
            socket.is_some()
        });
        let socket = socket.unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
    }

    #[test]
    fn forwards_bytes_and_preserves_half_close() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = target.local_addr().unwrap().port();
        let (host, client) = pair();
        host.allowed_port.store(port, Ordering::Release);
        let payload: Vec<u8> = (0..128_000).map(|n| (n % 251) as u8).collect();
        let expected = payload.clone();
        let service = thread::spawn(move || {
            target.set_nonblocking(true).unwrap();
            let mut socket = None;
            wait(|| {
                socket = target.accept().ok().map(|v| v.0);
                socket.is_some()
            });
            let mut socket = socket.unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
            socket.write_all(&bytes).unwrap();
        });
        let mut socket = connect(&client, port);
        socket.write_all(&payload).unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).unwrap();
        assert_eq!(response, payload);
        service.join().unwrap();
        client.close();
        host.close();
    }

    #[test]
    fn denies_unapproved_ports_and_revokes_open_connections() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let port = target.local_addr().unwrap().port();
        let (host, client) = pair();
        let mut denied = connect(&client, port);
        assert_eq!(denied.read(&mut [0; 1]).unwrap(), 0);
        assert!(target.accept().is_err());
        host.allowed_port.store(port, Ordering::Release);
        let mut approved =
            TcpStream::connect(("127.0.0.1", client.local_port.load(Ordering::Acquire))).unwrap();
        approved
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut server = None;
        wait(|| {
            server = target.accept().ok().map(|v| v.0);
            server.is_some()
        });
        host.allowed_port.store(0, Ordering::Release);
        assert_eq!(approved.read(&mut [0; 1]).unwrap(), 0);
        client.close();
        host.close();
    }

    #[test]
    fn access_request_requires_explicit_host_approval_and_revocation_is_announced() {
        let (host, client) = pair();
        client.request_access(8080);
        wait(|| host.pending_port.load(Ordering::Acquire) == 8080);
        assert_eq!(host.allowed_port.load(Ordering::Acquire), 0);
        assert_eq!(client.peer_allowed.load(Ordering::Acquire), 0);
        host.answer_access(false);
        wait(|| client.peer_rejected.load(Ordering::Acquire) == 8080);
        assert_eq!(host.allowed_port.load(Ordering::Acquire), 0);
        client.request_access(8080);
        wait(|| host.pending_port.load(Ordering::Acquire) == 8080);
        host.answer_access(true);
        wait(|| client.peer_allowed.load(Ordering::Acquire) == 8080);
        host.allowed_port.store(0, Ordering::Release);
        wait(|| client.peer_allowed.load(Ordering::Acquire) == 0);
        client.close();
        host.close();
    }

    #[test]
    fn simultaneous_connections_keep_their_data_separate_and_all_close_on_revocation() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let port = target.local_addr().unwrap().port();
        let (host, client) = pair();
        host.allowed_port.store(port, Ordering::Release);
        let mut a = connect(&client, port);
        let local = client.local_port.load(Ordering::Acquire);
        let mut b = TcpStream::connect(("127.0.0.1", local)).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut remote = Vec::new();
        wait(|| {
            while let Ok((socket, _)) = target.accept() {
                remote.push(socket);
            }
            remote.len() == 2
        });
        a.write_all(b"first").unwrap();
        b.write_all(b"other").unwrap();
        for socket in &mut remote {
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 5];
            socket.read_exact(&mut request).unwrap();
            socket.write_all(&request).unwrap();
        }
        let mut data = [0; 5];
        a.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"first");
        b.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"other");
        host.allowed_port.store(0, Ordering::Release);
        assert_eq!(a.read(&mut data).unwrap(), 0);
        assert_eq!(b.read(&mut data).unwrap(), 0);
        client.close();
        host.close();
    }

    #[test]
    fn validates_packet_sizes_before_queueing() {
        let tunnel = Tunnel::new();
        assert!(tunnel.receive(&[1]).is_err());
        assert!(
            tunnel
                .receive(&packet(DATA, 1, &vec![0; CHUNK + 1]))
                .is_err()
        );
        assert!(tunnel.receive(&packet(OPEN, 1, &[0, 80])).is_ok());
    }
}
