//! DHT buluşma döngüsü. `Role::Host` servis, `Role::Client` istemcidir.

use crate::key::Key;
use openocd_link_core::addr::candidates_text;
use openocd_link_core::log::log;
use openocd_link_core::punch::{self, Bridged, Node};
use openocd_link_core::rendezvous::{self, Record, Role};
use openocd_link_core::stun;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct ClientDial {
    pub tx: SyncSender<io::Result<Bridged>>,
    pub cancel: Arc<AtomicBool>,
}

pub struct Meet {
    reset: Arc<AtomicBool>,
    published_ok: Arc<Mutex<Instant>>,
    settled: Arc<AtomicBool>,
}

impl Meet {
    /// Oturum kuruldu: bağlantı arama çabası (çok port, yoklama) durdurulur
    pub fn settle(&self) {
        self.settled.store(true, Ordering::Release);
    }

    /// Denenmiş adres listesini temizler; aynı istemci yeniden bağlanabilir
    pub fn reset(&self) {
        self.reset.store(true, Ordering::Release);
    }

    /// Son başarılı DHT yayınından (hiç olmadıysa başlangıçtan) beri geçen süre
    pub fn since_publish(&self) -> Duration {
        self.published_ok.lock().unwrap().elapsed()
    }
}

/// Simetrik NAT'ta bir sonraki eşlemenin tahmini; NAT simetrik değilse `None`
fn probe_window() -> Option<SocketAddr> {
    let Some((addr, symmetric)) = stun::probe_nat() else {
        log("NAT türü ölçülemedi (STUN yanıt vermedi)");
        return None;
    };
    match symmetric {
        Some(true) => log(format!("NAT türü: simetrik (her hedefe ayrı port; son görülen {})", addr.port())),
        Some(false) => log("NAT türü: koni (dış port sabit)"),
        // Tek sunucu yanıt verdi: simetrik olabilir. Simetrik kabul edilir; karşı taraf
        // yalnız fazladan port yoklar, koni NAT'ta da bağlantı kurulur.
        None => log(format!(
            "NAT türü belirsiz (yalnız bir STUN sunucusu yanıt verdi); simetrik kabul ediliyor, görülen {}",
            addr.port()
        )),
    }
    (symmetric != Some(false)).then_some(addr)
}

/// Servis simetrik NAT arkasındayken açılan port yelpazesinin ömrü; istemci kaydını yeniledikçe uzar
const FAN_FOR: Duration = Duration::from_secs(150);
/// Bu yaştan genç yelpaze aynı hedef için yeniden açılmaz
const FAN_KEEP: Duration = Duration::from_secs(100);
/// Doğrudan yol bu sürede açılmazsa NAT ölçümüne bakılmadan çok port ve yoklama başlar
const ESCALATE_AFTER: Duration = Duration::from_secs(6);

/// `stop` true olunca döngü biter. İstemci, yol açılınca çağıran tarafından durdurulur.
pub fn start(
    node: Arc<Node>,
    key: &Key,
    role: Role,
    poll: Duration,
    dial: Option<ClientDial>,
    stop: Arc<AtomicBool>,
) -> Meet {
    let reset = Arc::new(AtomicBool::new(false));
    let keys = key.rendezvous();
    let flag = reset.clone();
    let published_ok = Arc::new(Mutex::new(Instant::now()));
    let ok_at = published_ok.clone();
    let settled = Arc::new(AtomicBool::new(false));
    let settled_flag = settled.clone();
    thread::spawn(move || {
        let host = matches!(role, Role::Host);
        // Servis vazgeçmez: açılışta ağ henüz yoksa gelene kadar yeniden dener
        let dht = loop {
            if let Some(dht) = rendezvous::shared() {
                break dht;
            }
            log("DHT'ye katılınamadı");
            if !host {
                return;
            }
            for _ in 0..50 {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
        };
        let other = if host { Role::Client } else { Role::Host };
        let (publish_every, fetch_every) = if host {
            (Duration::from_secs(120), poll)
        } else {
            (Duration::from_secs(60), Duration::from_secs(3))
        };
        let mut tried: Vec<SocketAddr> = Vec::new();
        let mut published: Option<Instant> = None;
        let mut last: Option<Vec<SocketAddr>> = None;
        let mut fetched: Option<Instant> = None;
        // Kendi NAT'ımız: simetrikse kayda yazılır, karşı taraf ona göre davranır
        let mut window: Option<SocketAddr> = probe_window();
        let mut punched_window: Option<SocketAddr> = None;
        let mut misses = 0u32;
        let mut last_stamp: Option<u64> = None;
        let mut fan: Option<(SocketAddr, Instant, Arc<AtomicBool>)> = None;
        let mut both_hard_logged = false;
        let mut peer_active: Option<Instant> = None;
        let mut seen: Option<(SocketAddr, Instant)> = None;
        let mut escalated: Option<SocketAddr> = None;
        while !stop.load(Ordering::Acquire) {
            if flag.swap(false, Ordering::AcqRel) {
                tried.clear();
                punched_window = None;
            }
            // Oturum kuruldu: açık port yelpazesi ve yoklama durur
            if settled_flag.swap(false, Ordering::AcqRel) {
                peer_active = None;
                if let Some((target, _, cancel)) = fan.take() {
                    cancel.store(true, Ordering::Release);
                    node.spray(target.ip(), Duration::ZERO);
                }
            }
            let current = node.candidates();
            // Ağ değişti: NAT türü de değişmiş olabilir
            if host && last.is_some() && current.is_some() && current != last {
                window = probe_window();
            }
            if (published.is_none_or(|t| t.elapsed() >= publish_every) || current != last)
                && let Some(candidates) = current
            {
                published = Some(Instant::now());
                last = Some(candidates.clone());
                let record = Record { candidates, window };
                if rendezvous::publish(dht, &keys, role, &record) {
                    *ok_at.lock().unwrap() = Instant::now();
                    log("Kayıt DHT'ye yazıldı");
                } else {
                    log("Kayıt DHT'ye yazılamadı; 15 sn sonra yeniden denenecek");
                    published = Instant::now().checked_sub(publish_every - Duration::from_secs(15));
                }
            }
            if fetched.is_none_or(|t| t.elapsed() >= fetch_every) {
                fetched = Some(Instant::now());
                let found = rendezvous::fetch_stamped(dht, &keys, other);
                if found.is_none() {
                    misses += 1;
                    if misses % 5 == 1 {
                        log(format!("Karşı kayıt okunamadı: {}", rendezvous::diagnose(dht, &keys, other)));
                    }
                }
                if let Some((stamp, found)) = found {
                    // Karşı taraf kaydını yenilediyse hâlâ arıyordur: ilk deneme tutmamış olabilir,
                    // aynı adreslere yeniden delik açılır (tek seferlik deneme bağlantıyı kaçırmasın)
                    let renewed = host && last_stamp.is_some_and(|s| s != stamp);
                    // Kayıt ilk kez görüldü ya da yenilendi: karşı taraf şu an arıyor
                    let active = last_stamp != Some(stamp);
                    last_stamp = Some(stamp);
                    let peer_public = found
                        .candidates
                        .iter()
                        .copied()
                        .find(|a| a.is_ipv4() && !punch::is_local(a.ip()));
                    if renewed {
                        punched_window = None;
                    }
                    let all = found.candidates.clone();
                    let new: Vec<SocketAddr> = found
                        .candidates
                        .into_iter()
                        .filter(|a| !tried.contains(a))
                        .collect();
                    if !new.is_empty() && !stop.load(Ordering::Acquire) {
                        log(format!("Karşının kaydı bulundu: {}", candidates_text(&new)));
                        tried.extend(&new);
                        match &dial {
                            Some(d) => {
                                if let Some(fresh) = probe_window() {
                                    window = Some(fresh);
                                    published = None;
                                }
                                node.dial(new, d.tx.clone(), d.cancel.clone());
                            }
                            None => node.punch(new),
                        }
                    } else if renewed && !stop.load(Ordering::Acquire) {
                        node.punch(all);
                    }
                    if dial.is_none()
                        && let Some(start) = found.window
                        && punched_window != Some(start)
                    {
                        punched_window = Some(start);
                        log(format!(
                            "Karşı simetrik NAT arkasında; {start} ve sonrasına delik açılıyor"
                        ));
                        node.punch_for(punch::window(start), Duration::from_secs(120));
                    }
                    // Simetrik NAT portu rastgele veriyorsa tahmin tutmaz: zor taraf çok port açar,
                    // öbür taraf rastgele portları yoklar; denk gelen porttan bağlantı kurulur
                    // NAT ölçümüne güvenilmez (STUN yanılabilir): doğrudan yol birkaç saniyede açılmadıysa
                    // ya da taraflardan biri simetrik görünüyorsa iki taraf da hem çok port açar hem
                    // karşının rastgele portlarını yoklar. Hangi taraf zor ağdaysa o yön tutar.
                    let Some(target) = peer_public else { continue };
                    if stop.load(Ordering::Acquire) {
                        continue;
                    }
                    if active {
                        peer_active = Some(Instant::now());
                    }
                    if seen.is_none_or(|(t, _)| t != target) {
                        seen = Some((target, Instant::now()));
                        escalated = None;
                    }
                    if window.is_some() && found.window.is_some() && !both_hard_logged {
                        both_hard_logged = true;
                        log("İki taraf da simetrik NAT arkasında görünüyor: doğrudan yol açılamayabilir. Açılmazsa taraflardan birini başka bir ağa (ev/ofis modemi) bağlayın");
                    }
                    // İstemci ararken hep etkindir; servis yalnız istemci kaydını yeniledikçe
                    let searching = dial.is_some() || peer_active.is_some_and(|t| t.elapsed() < FAN_FOR);
                    let hint = window.is_some() || found.window.is_some();
                    let waited = seen.is_some_and(|(_, at)| at.elapsed() >= ESCALATE_AFTER);
                    if !searching || !(hint || waited) {
                        continue;
                    }
                    if escalated != Some(target) {
                        escalated = Some(target);
                        log(format!(
                            "Doğrudan yol açılmadı; {} için çok port açılıyor ve rastgele portlar yoklanıyor",
                            target.ip()
                        ));
                        if let Some(d) = &dial {
                            node.fan_dial(target, d.tx.clone(), d.cancel.clone());
                        }
                    }
                    // Her okumada süre uzar; arama bitince kendiliğinden durur
                    node.spray(target.ip(), Duration::from_secs(15));
                    if dial.is_none() {
                        let keep = fan.as_ref().is_some_and(|(t, at, _)| *t == target && at.elapsed() < FAN_KEEP);
                        if !keep {
                            if let Some((_, _, old)) = fan.take() {
                                old.store(true, Ordering::Release);
                            }
                            let cancel = Arc::new(AtomicBool::new(false));
                            node.fan_back(target, FAN_FOR, cancel.clone());
                            fan = Some((target, Instant::now(), cancel));
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    Meet { reset, published_ok, settled }
}
