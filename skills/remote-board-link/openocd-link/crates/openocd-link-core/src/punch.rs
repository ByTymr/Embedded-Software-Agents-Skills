//! Sunucusuz internet bağlantısı: UDP delik açma ve QUIC (RFC 9000).
//!
//! İki taraf da aynı UDP soketinden birbirinin adaylarına paket gönderir; her modem dışarı giden
//! trafiğe yanıtı kabul ettiği için yol iki uçtan içeriden açılır, port yönlendirme gerekmez.
//! Dış IPv4 adresi STUN ile öğrenilir (yalnız adres sorgusu). QUIC akışı yerel bir TCP çiftine
//! köprülenir: doğrulama (SPAKE2 + Noise), onay ve ekran/giriş akışı `direct` modülünde aynen
//! çalışır. QUIC'in TLS sertifikası kendinden imzalıdır; kimlik doğrulama içerideki parola
//! el sıkışmasıyla yapılır, TLS yalnız taşıma katmanıdır.
use crate::stun;
use quinn::rustls;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ALPN: &[u8] = b"openocd-link/1";
/// Karşı tarafın kodu gelene kadar beklenecek en uzun süre
const DIAL_FOR: Duration = Duration::from_secs(900);
const PUNCH_FOR: Duration = Duration::from_secs(900);
/// Ev modemleri boşta kalan UDP eşlemesini genelde 30 sn–2 dk içinde kapatır
const KEEPALIVE: Duration = Duration::from_secs(20);
/// Simetrik NAT'ta tahmin edilen portun ardından denenecek port sayısı
const WINDOW: u16 = 96;
const MAX_TARGETS: usize = 32 + WINDOW as usize * 2;
const MAX_INCOMING: usize = 4;
/// Simetrik NAT portu rastgele veriyorsa tahmin tutmaz. Zor taraf bu kadar soket açar (her biri
/// NAT'ta ayrı bir dış port alır), öbür taraf rastgele portları yoklar: yoklamalardan biri açık
/// portlardan birine denk gelir (256 portta yoklama başına ~%0,4; saniyede 100 yoklamayla birkaç sn).
const FAN: usize = 256;
/// Yoklama hızı: 250 ms'de bir bu kadar paket
const SPRAY_PER_TICK: usize = 25;
const PROBE: &[u8] = b"openocd-link-punch";
/// Ters yön (servis istemciye bağlanır) akışının ilk baytı ve istemcinin kabul yanıtı
const BACK_MARK: u8 = b'R';
const BACK_OK: u8 = b'K';

enum Punch {
    Peers(Vec<SocketAddr>, Duration),
    Spray(IpAddr, Duration),
}

/// Ters yönde gelen bağlantının verileceği yer ve "ilk gelen kazanır" bayrağı
type BackSlot = (SyncSender<io::Result<Bridged>>, Arc<AtomicBool>);

enum FanMode {
    /// İstemci zor tarafta: denk gelen soketten servise bağlanır
    Dial(SyncSender<io::Result<Bridged>>),
    /// Servis zor tarafta: denk gelen soketten istemciye bağlanır, oturum servisin kabul kuyruğuna girer
    Back,
}

struct Fan {
    target: SocketAddr,
    lifetime: Duration,
    cancel: Arc<AtomicBool>,
    mode: FanMode,
}

/// Simetrik NAT'ın sıradaki eşlemeleri: ölçülen porttan biraz öncesi ve sonrası
pub fn window(start: SocketAddr) -> Vec<SocketAddr> {
    let from = start.port().saturating_sub(8).max(1);
    (from..=from.saturating_add(WINDOW))
        .map(|port| SocketAddr::new(start.ip(), port))
        .collect()
}

/// Uygulama boyunca yaşayan tek internet ucu: aynı UDP soketi hem bekleme hem bağlanma için
/// kullanılır ve modemdeki eşleme canlı tutulur. Böylece bu bilgisayarın kodu uygulama açık
/// kaldıkça değişmez; kayıtlı tanıdıklar kod yapıştırmadan yeniden bağlanabilir.
pub struct Node {
    /// Bu tarafın adayları; STUN bitince dolar (karşıya verilecek kod bunlardan oluşur)
    pub local: Arc<Mutex<Option<Vec<SocketAddr>>>>,
    punch: Sender<Punch>,
    /// Bekleme açıkken gelen bağlantılar buraya; kapalıyken reddedilir
    acceptor: Arc<Mutex<Option<SyncSender<Bridged>>>>,
    /// İstemcide: servisin ters yönde açtığı bağlantı buraya
    reverse: Arc<Mutex<Option<BackSlot>>>,
    dials: tokio::sync::mpsc::UnboundedSender<Dial>,
    fans: tokio::sync::mpsc::UnboundedSender<Fan>,
    stop: Arc<AtomicBool>,
}

struct Dial {
    peers: Vec<SocketAddr>,
    result: SyncSender<io::Result<Bridged>>,
    cancel: Arc<AtomicBool>,
    back: bool,
}

impl Node {
    pub fn candidates(&self) -> Option<Vec<SocketAddr>> {
        self.local.lock().unwrap().clone()
    }

    /// Karşı tarafın adaylarına delik açar (karşının kodu gelince ya da kayıtlı tanıdıklara)
    pub fn punch(&self, peers: Vec<SocketAddr>) {
        self.punch_for(peers, PUNCH_FOR);
    }

    pub fn punch_for(&self, peers: Vec<SocketAddr>, duration: Duration) {
        let _ = self.punch.send(Punch::Peers(peers, duration));
    }

    /// Karşı taraf simetrik NAT arkasında: `ip` adresinin rastgele portlarını `duration` boyunca yoklar
    pub fn spray(&self, ip: IpAddr, duration: Duration) {
        let _ = self.punch.send(Punch::Spray(ip, duration));
    }

    /// İstemci simetrik NAT arkasında: çok soket açar, servisin yoklaması denk gelen soketten bağlanır
    pub fn fan_dial(
        &self,
        target: SocketAddr,
        result: SyncSender<io::Result<Bridged>>,
        cancel: Arc<AtomicBool>,
    ) {
        let _ = self.fans.send(Fan { target, lifetime: DIAL_FOR, cancel, mode: FanMode::Dial(result) });
    }

    /// Servis simetrik NAT arkasında: çok soket açar, istemcinin yoklaması denk gelen soketten
    /// istemciye bağlanır; oturum `accept` kuyruğuna gelen bağlantı gibi girer
    pub fn fan_back(&self, target: SocketAddr, lifetime: Duration, cancel: Arc<AtomicBool>) {
        let _ = self.fans.send(Fan { target, lifetime, cancel, mode: FanMode::Back });
    }

    /// İstemcide ters yönde gelen bağlantıyı kabul eder; `None` ile kapanır. `claimed` bayrağını
    /// ilk kuran yol alır (`dial` ile aynı bayrak verilirse tek bağlantı teslim edilir).
    pub fn accept_back(&self, slot: Option<(SyncSender<io::Result<Bridged>>, Arc<AtomicBool>)>) {
        *self.reverse.lock().unwrap() = slot;
    }

    /// Gelen bağlantıları `tx`'e verir; `None` ile bekleme kapanır
    pub fn accept(&self, tx: Option<SyncSender<Bridged>>) {
        *self.acceptor.lock().unwrap() = tx;
    }

    /// Karşının adaylarını aynı anda dener; karşı da delik açınca yol kurulur. `cancel` ile durur.
    pub fn dial(
        &self,
        peers: Vec<SocketAddr>,
        result: SyncSender<io::Result<Bridged>>,
        cancel: Arc<AtomicBool>,
    ) {
        self.queue_dial(peers, result, cancel, false);
    }

    /// Servisten sabit istemci adresine, ters yön işaretini göndererek bağlanır.
    pub fn dial_back(
        &self,
        peers: Vec<SocketAddr>,
        result: SyncSender<io::Result<Bridged>>,
        cancel: Arc<AtomicBool>,
    ) {
        self.queue_dial(peers, result, cancel, true);
    }

    fn queue_dial(
        &self,
        peers: Vec<SocketAddr>,
        result: SyncSender<io::Result<Bridged>>,
        cancel: Arc<AtomicBool>,
        back: bool,
    ) {
        self.punch(peers.clone());
        let _ = self.dials.send(Dial {
            peers,
            result,
            cancel,
            back,
        });
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

pub type Bridged = (TcpStream, SocketAddr);

/// `port` meşgulse rastgele port kullanılır. `own`: adaylara eklenecek adresler; `None` ise
/// bilgisayarın adresleri kendiliğinden bulunur ve ağ değişimi izlenir (Wi-Fi değişti, modem
/// yeniden bağlandı): yeni ağda soket yenilenir, dış adres yeniden öğrenilir, kod güncellenir.
pub fn start(port: u16, own: Option<Vec<IpAddr>>) -> Arc<Node> {
    let local = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let acceptor = Arc::new(Mutex::new(None::<SyncSender<Bridged>>));
    let (punch_tx, punch_rx) = mpsc::channel();
    let (dial_tx, mut dial_rx) = tokio::sync::mpsc::unbounded_channel::<Dial>();
    let (rebind_tx, mut rebind_rx) = tokio::sync::mpsc::unbounded_channel::<UdpSocket>();
    let (fan_tx, mut fan_rx) = tokio::sync::mpsc::unbounded_channel::<Fan>();
    let reverse = Arc::new(Mutex::new(None::<BackSlot>));
    let node = Arc::new(Node {
        local: local.clone(),
        punch: punch_tx,
        acceptor: acceptor.clone(),
        reverse: reverse.clone(),
        dials: dial_tx,
        fans: fan_tx,
        stop: stop.clone(),
    });
    thread::spawn(move || {
        let watch = own.is_none();
        let own_now = || own.clone().unwrap_or_else(local_addresses);
        let Ok(socket) = bind(port) else {
            *local.lock().unwrap() = Some(Vec::new());
            return;
        };
        let mut addresses = own_now();
        *local.lock().unwrap() = Some(candidates(&socket, &addresses));
        let Ok(sender) = socket.try_clone() else {
            return;
        };
        let sender = Arc::new(Mutex::new(sender));
        let dual = socket.local_addr().is_ok_and(|a| a.is_ipv6());
        {
            let (sender, stop) = (sender.clone(), stop.clone());
            thread::spawn(move || punch_loop(&sender, dual, &punch_rx, &stop));
        }
        if watch {
            let (local, sender, stop) = (local.clone(), sender.clone(), stop.clone());
            thread::spawn(move || {
                let mut public = local
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|c| c.first().map(|a| a.ip()));
                let mut checked = Instant::now();
                while !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_secs(3));
                    let now = local_addresses();
                    // Yerel adres değişmeden dış adres de değişebilir (modem yeniden bağlandı)
                    let moved = if now != addresses {
                        true
                    } else if checked.elapsed() >= Duration::from_secs(60) {
                        checked = Instant::now();
                        let probe = UdpSocket::bind("0.0.0.0:0")
                            .ok()
                            .and_then(|s| stun::mapped_v4(&s));
                        probe.is_some_and(|p| Some(p.ip()) != public)
                    } else {
                        false
                    };
                    if !moved {
                        continue;
                    }
                    let Ok(fresh) = bind(0) else { continue };
                    addresses = now;
                    let list = candidates(&fresh, &addresses);
                    public = list.first().map(|a| a.ip());
                    let (Ok(clone), Ok(quic)) = (fresh.try_clone(), fresh.try_clone()) else {
                        continue;
                    };
                    *sender.lock().unwrap() = clone;
                    let _ = rebind_tx.send(quic);
                    crate::log::log(format!(
                        "Ağ değişti; yeni bağlantı kodu: {}",
                        crate::addr::candidates_text(&list)
                    ));
                    *local.lock().unwrap() = Some(list);
                }
            });
        }
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let Ok(endpoint) = quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                server_config(),
                socket,
                Arc::new(quinn::TokioRuntime),
            ) else {
                return;
            };
            let incoming = {
                let (endpoint, acceptor, stop) = (endpoint.clone(), acceptor.clone(), stop.clone());
                tokio::spawn(async move { accept_loop(endpoint, acceptor, reverse, stop).await })
            };
            let config = client_config();
            while !stop.load(Ordering::Acquire) {
                tokio::select! {
                    dial = dial_rx.recv() => match dial {
                        Some(dial) => {
                            tokio::spawn(run_dial(endpoint.clone(), config.clone(), dial, stop.clone()));
                        }
                        None => break,
                    },
                    Some(fan) = fan_rx.recv() => {
                        tokio::spawn(run_fan(acceptor.clone(), config.clone(), fan, stop.clone()));
                    }
                    // Süren bağlantılar yeni sokete taşınır (QUIC bağlantı göçü)
                    Some(fresh) = rebind_rx.recv() => {
                        let _ = endpoint.rebind(fresh);
                    }
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
            }
            incoming.abort();
            endpoint.close(0u32.into(), b"bitti");
        });
    });
    node
}

/// Soketin adayları: STUN ile dış adres (soket QUIC'e verilmeden ya da yenisi kullanılmadan
/// önce), ardından bilgisayarın kendi adresleri aynı portla
fn candidates(socket: &UdpSocket, own: &[IpAddr]) -> Vec<SocketAddr> {
    let port = socket.local_addr().map_or(0, |a| a.port());
    let mut list = Vec::new();
    if let Some(mapped) = stun::mapped_v4(socket) {
        list.push(mapped);
    }
    for ip in own {
        let addr = SocketAddr::new(*ip, port);
        if !list.contains(&addr) {
            list.push(addr);
        }
    }
    list
}

async fn accept_loop(
    endpoint: quinn::Endpoint,
    acceptor: Arc<Mutex<Option<SyncSender<Bridged>>>>,
    reverse: Arc<Mutex<Option<BackSlot>>>,
    stop: Arc<AtomicBool>,
) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_INCOMING));
    while !stop.load(Ordering::Acquire) {
        let next = tokio::time::timeout(Duration::from_millis(300), endpoint.accept());
        let Ok(Some(incoming)) = next.await else {
            continue;
        };
        // Bekleme kapalıyken gelen bağlantı el sıkışmadan reddedilir
        let forward = acceptor.lock().unwrap().clone();
        let back = match forward {
            Some(_) => None,
            None => reverse.lock().unwrap().clone(),
        };
        if forward.is_none() && back.is_none() {
            incoming.refuse();
            continue;
        }
        // Önce adres sahipliği, sonra sınırlı sayıda TLS/akış işçisi.
        if !incoming.remote_address_validated() {
            let _ = incoming.retry();
            continue;
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        tokio::spawn(async move {
            let _slot = slot;
            let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(5), incoming).await else {
                return;
            };
            let remote = conn.remote_address();
            let Ok(Ok((mut send, mut recv))) =
                tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await
            else {
                conn.close(1u32.into(), b"stream timeout");
                return;
            };
            if let Some(tx) = forward {
                let _ = bridge(conn, send, recv, remote, |pair| tx.try_send(pair).is_ok()).await;
            } else if let Some((result, claimed)) = back {
                // Ters yön: servis açtı. İşaret baytı doğruysa ve başka yol kurulmadıysa kabul edilir.
                let mut mark = [0u8; 1];
                let marked =
                    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut mark)).await;
                if !matches!(marked, Ok(Ok(())))
                    || mark[0] != BACK_MARK
                    || claimed.swap(true, Ordering::AcqRel)
                {
                    conn.close(1u32.into(), b"mesgul");
                    return;
                }
                if send.write_all(&[BACK_OK]).await.is_err() {
                    return;
                }
                crate::log::log(format!(
                    "Servis ters yönden bağlandı ({})",
                    crate::addr::show_addr(remote)
                ));
                let _ = bridge(conn, send, recv, remote, |pair| {
                    result.try_send(Ok(pair)).is_ok()
                })
                .await;
            }
        });
    }
}

async fn run_dial(
    endpoint: quinn::Endpoint,
    config: quinn::ClientConfig,
    dial: Dial,
    stop: Arc<AtomicBool>,
) {
    let Dial {
        peers,
        result,
        cancel,
        back,
    } = dial;
    let deadline = Instant::now() + DIAL_FOR;
    let winner = loop {
        if stop.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
            return;
        }
        if Instant::now() >= deadline {
            let _ = result.try_send(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "İnternet üzerinden yol açılamadı. İki taraf da paylaşımlı adres (CGNAT) arkasında olabilir.",
            )));
            return;
        }
        let mut attempts = tokio::task::JoinSet::new();
        for &peer in &peers {
            let (endpoint, config) = (endpoint.clone(), config.clone());
            attempts.spawn(async move {
                let conn = endpoint
                    .connect_with(config, peer, "openocd-link")
                    .ok()?
                    .await
                    .ok()?;
                let (mut send, mut recv) = conn.open_bi().await.ok()?;
                if back {
                    reverse_handshake(&mut send, &mut recv).await?;
                }
                Some((conn, send, recv, peer))
            });
        }
        let mut found = None;
        loop {
            // İptal, süren denemeleri beklemeden işlenir
            if stop.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining.min(Duration::from_millis(300)), attempts.join_next()).await {
                Ok(Some(Ok(Some(won)))) => {
                    found = Some(won);
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) if cancel.load(Ordering::Acquire) => break,
                Err(_) => {}
            }
        }
        attempts.abort_all();
        if let Some(won) = found {
            break won;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let (conn, send, recv, peer) = winner;
    // İlk kurulan yol kazanır: aynı bayrağı paylaşan öbür denemeler (yelpaze, ters yön) durur
    if cancel.swap(true, Ordering::AcqRel) {
        conn.close(0u32.into(), b"iptal");
        return;
    }
    let _ = bridge(conn, send, recv, peer, |pair| {
        result.try_send(Ok(pair)).is_ok()
    })
    .await;
}

async fn reverse_handshake(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) -> Option<()> {
    // QUIC karşı tarafı ilk veri gönderilene kadar akıştan haberdar etmez.
    send.write_all(&[BACK_MARK]).await.ok()?;
    let mut reply = [0u8; 1];
    recv.read_exact(&mut reply).await.ok()?;
    (reply[0] == BACK_OK).then_some(())
}

/// Simetrik NAT'ın arkasındaki taraf: `FAN` soket açıp her birinden karşıya paket yollar (NAT her
/// birine ayrı dış port verir). Karşının rastgele yoklaması bu portlardan birine denk gelince o
/// soket QUIC ucuna çevrilir ve yoklamanın geldiği adrese bağlanılır: yol iki yönde de açıktır.
async fn run_fan(
    acceptor: Arc<Mutex<Option<SyncSender<Bridged>>>>,
    config: quinn::ClientConfig,
    fan: Fan,
    stop: Arc<AtomicBool>,
) {
    let Fan { target, lifetime, cancel, mode } = fan;
    let back = matches!(mode, FanMode::Back);
    // Yedek kopya QUIC'e verilir; tokio kopyası yalnız yoklamayı bekler
    let mut sockets: Vec<(Option<UdpSocket>, Arc<tokio::net::UdpSocket>)> = Vec::new();
    for _ in 0..FAN {
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
            break;
        };
        let (Ok(()), Ok(spare)) = (socket.set_nonblocking(true), socket.try_clone()) else {
            continue;
        };
        if let Ok(socket) = tokio::net::UdpSocket::from_std(socket) {
            sockets.push((Some(spare), Arc::new(socket)));
        }
    }
    if sockets.is_empty() {
        return;
    }
    crate::log::log(format!(
        "Simetrik NAT: {} port açıldı, karşının yoklaması bekleniyor ({target})",
        sockets.len()
    ));
    let (hit_tx, mut hit_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, SocketAddr)>();
    let mut waiters = Vec::new();
    for (i, (_, socket)) in sockets.iter().enumerate() {
        let (socket, hit_tx) = (socket.clone(), hit_tx.clone());
        waiters.push(tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, from)) if from.ip() == target.ip() && &buf[..n] == PROBE => {
                        let _ = hit_tx.send((i, from));
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                }
            }
        }));
    }
    let deadline = Instant::now() + lifetime;
    let mut refreshed: Option<Instant> = None;
    let won = loop {
        if stop.load(Ordering::Acquire)
            || cancel.load(Ordering::Acquire)
            || Instant::now() >= deadline
        {
            break None;
        }
        // Eşlemeler NAT'ta canlı kalsın
        if refreshed.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            refreshed = Some(Instant::now());
            for (_, socket) in &sockets {
                let _ = socket.try_send_to(PROBE, target);
            }
        }
        let hit = tokio::time::timeout(Duration::from_millis(300), hit_rx.recv()).await;
        let Ok(Some((i, from))) = hit else {
            continue;
        };
        let Some(spare) = sockets[i].0.take() else {
            continue;
        };
        crate::log::log(format!("Karşının yoklaması denk geldi ({from}); bağlanılıyor"));
        let Ok(endpoint) = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            spare,
            Arc::new(quinn::TokioRuntime),
        ) else {
            continue;
        };
        let attempt = async {
            let conn = endpoint
                .connect_with(config.clone(), from, "openocd-link")
                .ok()?
                .await
                .ok()?;
            let (mut send, mut recv) = conn.open_bi().await.ok()?;
            if back {
                reverse_handshake(&mut send, &mut recv).await?;
            }
            Some((conn, send, recv))
        };
        match tokio::time::timeout(Duration::from_secs(10), attempt).await {
            Ok(Some(streams)) => break Some((endpoint, streams, from)),
            _ => endpoint.close(0u32.into(), b"olmadi"),
        }
    };
    for waiter in waiters {
        waiter.abort();
    }
    drop(sockets);
    let Some((endpoint, (conn, send, recv), from)) = won else {
        return;
    };
    match mode {
        FanMode::Dial(result) => {
            if cancel.swap(true, Ordering::AcqRel) {
                conn.close(0u32.into(), b"iptal");
            } else {
                let _ = bridge(conn, send, recv, from, |pair| {
                    result.try_send(Ok(pair)).is_ok()
                })
                .await;
            }
        }
        FanMode::Back => {
            let tx = acceptor.lock().unwrap().clone();
            match tx {
                Some(tx) => {
                    let _ =
                        bridge(conn, send, recv, from, |pair| tx.try_send(pair).is_ok()).await;
                }
                None => conn.close(1u32.into(), b"mesgul"),
            }
        }
    }
    endpoint.wait_idle().await;
}

/// Karşının adaylarına küçük paketler: kendi modemimizde karşıya giden yolu açar. QUIC bu
/// paketleri geçersiz sayıp yok sayar. Ayrıca bir STUN noktasına düzenli paket gider: modem
/// eşlemeyi kapatmaz, bu bilgisayarın kodu geçerli kalır.
fn punch_loop(
    socket: &Mutex<UdpSocket>,
    dual: bool,
    peers: &Receiver<Punch>,
    stop: &AtomicBool,
) {
    let mut spray: Option<(IpAddr, Instant)> = None;
    let map = |peer: SocketAddr| match (dual, peer.ip()) {
        (true, IpAddr::V4(v4)) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), peer.port()),
        _ => peer,
    };
    let keepalive = stun::keepalive_target().map(map);
    let mut last_keepalive = Instant::now();
    let mut targets: Vec<(SocketAddr, Instant)> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        while let Ok(message) = peers.try_recv() {
            let (more, duration) = match message {
                Punch::Peers(more, duration) => (more, duration),
                Punch::Spray(ip, duration) => {
                    spray = Some((ip, Instant::now() + duration));
                    continue;
                }
            };
            let until = Instant::now() + duration;
            for peer in more.into_iter().map(map) {
                match targets.iter().position(|(t, _)| *t == peer) {
                    Some(i) => targets[i].1 = until,
                    None if targets.len() < MAX_TARGETS => targets.push((peer, until)),
                    None => {}
                }
            }
        }
        let now = Instant::now();
        targets.retain(|(_, until)| *until > now);
        let socket = socket.lock().unwrap();
        for (target, _) in &targets {
            let _ = socket.send_to(PROBE, target);
        }
        if spray.is_some_and(|(_, until)| until <= now) {
            spray = None;
        }
        // Karşının simetrik NAT'ında açık duran portlardan birine denk gelene kadar rastgele yoklama
        let mut random = [0u8; SPRAY_PER_TICK * 2];
        if let Some((ip, _)) = spray
            && getrandom::fill(&mut random).is_ok()
        {
            for pair in random.chunks_exact(2) {
                let port = 1024 + u16::from_be_bytes([pair[0], pair[1]]) % 64512;
                let _ = socket.send_to(PROBE, map(SocketAddr::new(ip, port)));
            }
        }
        if let Some(target) = keepalive
            && last_keepalive.elapsed() >= KEEPALIVE
        {
            last_keepalive = Instant::now();
            let _ = socket.send_to(&stun::binding_request(), target);
        }
        drop(socket);
        thread::sleep(Duration::from_millis(250));
    }
}

/// Mümkünse IPv4 ve IPv6'yı birlikte taşıyan tek soket
fn bind(port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let dual = (|| {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port).into())?;
        io::Result::Ok(UdpSocket::from(s))
    })();
    dual.or_else(|_| UdpSocket::bind(("0.0.0.0", port)))
        .or_else(|_| UdpSocket::bind(("0.0.0.0", 0)))
}

/// QUIC akışını yerel bir TCP çiftine bağlar; `deliver` çiftin öbür ucunu `direct`'e verir.
async fn bridge(
    conn: quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    remote: SocketAddr,
    deliver: impl FnOnce(Bridged) -> bool,
) -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let outer = TcpStream::connect(listener.local_addr()?)?;
    let (inner, _) = listener.accept()?;
    // Küçük giriş/yanıt paketleri yerel TCP üzerinde Nagle beklemesine takılmasın.
    outer.set_nodelay(true)?;
    inner.set_nodelay(true)?;
    if !deliver((outer, remote)) {
        conn.close(1u32.into(), b"mesgul");
        return Ok(());
    }
    inner.set_nonblocking(true)?;
    let inner = tokio::net::TcpStream::from_std(inner)?;
    let (mut read, mut write) = inner.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut read, &mut send).await;
        let _ = send.finish();
    };
    let down = async {
        let _ = tokio::io::copy(&mut recv, &mut write).await;
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
    };
    tokio::join!(up, down);
    if let Some(reason) = conn.close_reason() {
        crate::log::log(format!(
            "İnternet bağlantısı kapandı ({}): {reason}",
            crate::addr::show_addr(remote)
        ));
    } else {
        crate::log::log(format!(
            "İnternet bağlantısı akışı bitti ({})",
            crate::addr::show_addr(remote)
        ));
    }
    conn.close(0u32.into(), b"bitti");
    Ok(())
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    Arc::new(t)
}

fn server_config() -> Option<quinn::ServerConfig> {
    let key = rcgen::generate_simple_self_signed(vec!["openocd-link".into()]).ok()?;
    let cert = key.cert.der().clone();
    let private = rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .ok()?
    .with_no_client_auth()
    .with_single_cert(vec![cert], private.into())
    .ok()?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).ok()?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    Some(config)
}

fn client_config() -> quinn::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC TLS");
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport());
    config
}

/// Sertifikanın imzası doğrulanır ama kime ait olduğu sorulmaz: karşı tarafın kimliğini
/// içerideki parola el sıkışması (SPAKE2 + Noise) kanıtlar.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local()
                // CGNAT 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Varsayılan rotanın kaynak adresleri. UDP `connect` paket göndermez; yalnız işletim
/// sisteminin hangi arayüzü seçeceğini sorar. Genel IPv6 adresi önce gelir: karşıdan
/// doğrudan erişilebilen odur, IPv4 çoğu zaman modem arkasındadır.
pub fn local_addresses() -> Vec<IpAddr> {
    let probe = |bind: &str, target: &str| {
        let socket = UdpSocket::bind(bind).ok()?;
        socket.connect(target).ok()?;
        Some(socket.local_addr().ok()?.ip())
    };
    let mut out: Vec<IpAddr> = [
        probe("0.0.0.0:0", "192.0.2.1:9"),
        probe("[::]:0", "[2001:db8::1]:9"),
    ]
    .into_iter()
    .flatten()
    .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
    .map(|ip| match ip {
        IpAddr::V6(_) => stable_ipv6().unwrap_or(ip),
        _ => ip,
    })
    .collect();
    out.sort_by_key(|ip| is_local(*ip));
    out
}

/// Giden bağlantı için seçilen IPv6 çoğu zaman gizlilik amaçlı geçici adrestir ve gün içinde
/// değişir; paylaşılacak olan, aynı arayüzün kalıcı genel adresidir.
#[cfg(target_os = "linux")]
fn stable_ipv6() -> Option<IpAddr> {
    const TEMPORARY: u8 = 0x01;
    const DEPRECATED: u8 = 0x20;
    let text = std::fs::read_to_string("/proc/net/if_inet6").ok()?;
    text.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (hex, scope, flags) = (f.first()?, f.get(3)?, f.get(4)?);
        let flags = u8::from_str_radix(flags, 16).ok()?;
        if *scope != "00" || flags & (TEMPORARY | DEPRECATED) != 0 || hex.len() != 32 {
            return None;
        }
        let ip = IpAddr::V6(u128::from_str_radix(hex, 16).ok()?.into());
        (!is_local(ip)).then_some(ip)
    })
}

#[cfg(not(target_os = "linux"))]
fn stable_ipv6() -> Option<IpAddr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn unauthenticated_connections_are_bounded_and_slots_recover() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let server = quinn::Endpoint::server(
                    server_config().unwrap(),
                    "127.0.0.1:0".parse().unwrap(),
                )
                .unwrap();
                let address = server.local_addr().unwrap();
                let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
                let (tx, _rx) = mpsc::sync_channel(1);
                let stop = Arc::new(AtomicBool::new(false));
                let worker = tokio::spawn(accept_loop(
                    server.clone(),
                    Arc::new(Mutex::new(Some(tx))),
                    Arc::default(),
                    stop.clone(),
                ));
                let mut parked = Vec::new();
                for _ in 0..MAX_INCOMING {
                    let connection = tokio::time::timeout(
                        Duration::from_secs(2),
                        client
                            .connect_with(client_config(), address, "openocd-link")
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    parked.push(connection);
                }
                // TLS tamamlandı ama uygulama akışı gelmedi: daha fazla işçi açılmaz.
                let extra = tokio::time::timeout(
                    Duration::from_secs(2),
                    client
                        .connect_with(client_config(), address, "openocd-link")
                        .unwrap(),
                )
                .await
                .unwrap();
                assert!(extra.is_err());
                for connection in parked {
                    connection.close(0u32.into(), b"test done");
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let next = tokio::time::timeout(
                        Duration::from_secs(1),
                        client
                            .connect_with(client_config(), address, "openocd-link")
                            .unwrap(),
                    )
                    .await;
                    if let Ok(Ok(connection)) = next {
                        connection.close(0u32.into(), b"test done");
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "kapanan bağlantıların yuvaları geri verilmedi"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                stop.store(true, Ordering::Release);
                worker.await.unwrap();
                server.close(0u32.into(), b"test done");
                client.close(0u32.into(), b"test done");
            });
    }

    fn loopback_target(node: &Node) -> SocketAddr {
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(list) = node.candidates() {
                return list.into_iter().find(|a| a.ip() == loopback).unwrap();
            }
            assert!(Instant::now() < until, "aday listesi gelmedi");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn exchange(mut a: TcpStream, mut b: TcpStream) {
        a.write_all(b"merhaba").unwrap();
        let mut buf = [0u8; 7];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"merhaba");
        b.write_all(b"selam").unwrap();
        let mut buf = [0u8; 5];
        a.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"selam");
    }

    /// Yelpaze testleri aynı anda koşarsa birinin yoklaması öbürünün soketine denk gelir
    /// (ikisi de 127.0.0.1): sırayla çalışırlar.
    static FAN_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn fixed_reverse_dial_opens_stream_before_application_writes() {
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let client = start(0, Some(vec![loopback]));
        let (client_tx, client_rx) = mpsc::sync_channel(1);
        client.accept_back(Some((client_tx, Arc::default())));
        let target = loopback_target(&client);
        let server = start(0, Some(vec![]));
        let (server_tx, server_rx) = mpsc::sync_channel(1);
        server.dial_back(vec![target], server_tx, Arc::default());

        let (client_stream, _) = client_rx.recv_timeout(Duration::from_secs(15)).unwrap().unwrap();
        let (server_stream, _) = server_rx.recv_timeout(Duration::from_secs(15)).unwrap().unwrap();
        exchange(client_stream, server_stream);
    }

    /// İstemci simetrik NAT arkasında: yelpazesindeki bir sokete servisin rastgele yoklaması denk gelir
    #[test]
    fn fan_dial_connects_from_the_socket_a_random_probe_hits() {
        let _serial = FAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let (in_tx, in_rx) = mpsc::sync_channel(1);
        let server = start(0, Some(vec![loopback]));
        server.accept(Some(in_tx));
        let target = loopback_target(&server);
        let (out_tx, out_rx) = mpsc::sync_channel(1);
        let client = start(0, Some(vec![]));
        client.fan_dial(target, out_tx, Arc::default());
        server.spray(loopback, Duration::from_secs(90));
        let (a, _) = out_rx.recv_timeout(Duration::from_secs(90)).unwrap().unwrap();
        // Servis akışı ilk veriyle görür
        let writer = thread::spawn(move || {
            let (b, _) = in_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            b
        });
        let mut a = a;
        a.write_all(b"x").unwrap();
        let mut b = writer.join().unwrap();
        let mut first = [0u8; 1];
        b.read_exact(&mut first).unwrap();
        exchange(a, b);
    }

    /// Servis simetrik NAT arkasında: yelpazesinden istemciye ters yönde bağlanır
    #[test]
    fn fan_back_reaches_the_client_and_lands_in_the_accept_queue() {
        let _serial = FAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let (out_tx, out_rx) = mpsc::sync_channel(1);
        let client = start(0, Some(vec![loopback]));
        let claimed = Arc::new(AtomicBool::new(false));
        client.accept_back(Some((out_tx, claimed.clone())));
        let target = loopback_target(&client);
        let (in_tx, in_rx) = mpsc::sync_channel(1);
        let server = start(0, Some(vec![]));
        server.accept(Some(in_tx));
        server.fan_back(target, Duration::from_secs(90), Arc::default());
        client.spray(loopback, Duration::from_secs(90));
        let (a, _) = out_rx.recv_timeout(Duration::from_secs(90)).unwrap().unwrap();
        let (b, _) = in_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(claimed.load(Ordering::Acquire));
        exchange(a, b);
    }

    #[test]
    fn quic_bridge_carries_bytes_both_ways() {
        let loopback = IpAddr::from([127, 0, 0, 1]);
        let (in_tx, in_rx) = mpsc::sync_channel(1);
        let server = start(0, Some(vec![loopback]));
        server.accept(Some(in_tx));
        let until = Instant::now() + Duration::from_secs(10);
        let target = loop {
            if let Some(list) = server.candidates() {
                break list.into_iter().find(|a| a.ip() == loopback).unwrap();
            }
            assert!(Instant::now() < until, "aday listesi gelmedi");
            thread::sleep(Duration::from_millis(50));
        };
        let (out_tx, out_rx) = mpsc::sync_channel(1);
        let client = start(0, Some(vec![]));
        client.dial(vec![target], out_tx, Arc::default());
        let (mut a, _) = out_rx
            .recv_timeout(Duration::from_secs(15))
            .unwrap()
            .unwrap();
        a.write_all(b"merhaba").unwrap();
        let (mut b, _) = in_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let mut buf = [0u8; 7];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"merhaba");
        b.write_all(b"selam").unwrap();
        let mut buf = [0u8; 5];
        a.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"selam");
    }
}
