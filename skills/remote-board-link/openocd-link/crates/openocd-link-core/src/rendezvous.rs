//! Kod alışverişini kendiliğinden yapan buluşma noktası: BitTorrent DHT (BEP 44 imzalı kayıt).
//!
//! Sunucu yoktur; DHT, kimsenin sahibi olmadığı eşler arası ağdır. İki taraf da internet
//! adaylarını (bağlantı kodunu) aynı kullanıcı adı ve paroladan türeyen anahtarla imzalayıp
//! şifreleyerek bırakır; öbür taraf okuyup delik açar. Ağdaki eşler yalnız şifreli veri ve
//! gönderenin IP adresini görür. Anahtar herkese açık olduğundan türetme kasıtlı olarak yavaştır:
//! zayıf bir parolayı DHT'den tahmin etmek pahalı olsun. DHT erişilemezse kod yapıştırma kalır.
use blake2::{Blake2s256, Digest};
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit};
use mainline::{Dht, MutableItem, SigningKey};
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bu yaştan eski kayıt yok sayılır: kapanmış bir oturumun adresine delik açılmasın
const FRESH_SECS: u64 = 6 * 60 * 60;
/// Küçük saat farkına izin ver; uzak gelecekteki kayıtlar süresiz taze kalmasın.
const CLOCK_SKEW_SECS: u64 = 60;
/// Türetme turu: tek anahtar için ~0,1 sn, sözlük denemesi için çok pahalı
const ROUNDS: u32 = 1 << 18;

#[derive(Clone, Copy)]
pub enum Role {
    Host,
    Client,
}

impl Role {
    fn salt(self) -> &'static [u8] {
        match self {
            Role::Host => b"openocd-link-host",
            Role::Client => b"openocd-link-client",
        }
    }
}

/// Uygulama boyunca tek DHT istemcisi; ilk kullanımda arka planda ağa katılır.
/// Başarısız deneme saklanmaz: ağ sonradan gelirse sonraki çağrı yeniden dener.
pub fn shared() -> Option<&'static Dht> {
    static DHT: OnceLock<Dht> = OnceLock::new();
    if let Some(dht) = DHT.get() {
        return Some(dht);
    }
    let dht = Dht::client().ok()?;
    Some(DHT.get_or_init(|| dht))
}

pub struct Keys {
    signer: SigningKey,
    cipher: ChaCha20Poly1305,
}

/// İki taraf da aynı bağlam, kullanıcı adı ve paroladan aynı anahtarları türetir
pub fn keys_in(context: &[u8], username: &str, password: &str) -> Keys {
    let mut state: [u8; 32] = Blake2s256::new()
        .chain_update(context)
        .chain_update([username.len() as u8])
        .chain_update(username.as_bytes())
        .chain_update(password.as_bytes())
        .finalize()
        .into();
    for _ in 0..ROUNDS {
        state = Blake2s256::digest(state).into();
    }
    let derive = |label: &[u8]| -> [u8; 32] {
        Blake2s256::new()
            .chain_update(label)
            .chain_update(state)
            .finalize()
            .into()
    };
    Keys {
        signer: SigningKey::from_bytes(&derive(b"sign")),
        cipher: ChaCha20Poly1305::new(&derive(b"seal").into()),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn fresh_at(timestamp: u64, current: u64) -> bool {
    timestamp <= current.saturating_add(CLOCK_SKEW_SECS)
        && current.saturating_sub(timestamp) <= FRESH_SECS
}

/// Karşıya bırakılan kayıt: adaylar ve simetrik NAT'ta tahmin edilen port aralığının başı
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub candidates: Vec<SocketAddr>,
    pub window: Option<SocketAddr>,
}

fn seal(keys: &Keys, record: &Record) -> Option<Vec<u8>> {
    let mut text = format!(
        "{}\n{}",
        now(),
        crate::addr::candidates_text(&record.candidates)
    );
    if let Some(window) = record.window {
        text.push_str(&format!("\n{window}"));
    }
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    let mut out = nonce.to_vec();
    out.extend(keys.cipher.encrypt(&nonce.into(), text.as_bytes()).ok()?);
    Some(out)
}

#[cfg(test)]
fn open(keys: &Keys, value: &[u8]) -> Option<Record> {
    let (timestamp, record) = open_raw(keys, value)?;
    fresh_at(timestamp, now()).then_some(record)
}

/// Tazelik denetimi olmadan açar: kayıt zamanı ve içerik
fn open_raw(keys: &Keys, value: &[u8]) -> Option<(u64, Record)> {
    let (nonce, body) = value.split_at_checked(12)?;
    let nonce: [u8; 12] = nonce.try_into().ok()?;
    let text = keys.cipher.decrypt(&nonce.into(), body).ok()?;
    let text = String::from_utf8(text).ok()?;
    let mut lines = text.lines();
    let timestamp = lines.next()?.parse::<u64>().ok()?;
    let candidates = crate::addr::parse_list(lines.next().unwrap_or(""))
        .ok()
        .filter(|l| !l.is_empty())?;
    let window = lines.next().and_then(|w| w.parse().ok());
    Some((timestamp, Record { candidates, window }))
}

/// Karşı kaydın neden bulunamadığını Türkçe bir cümleyle açıklar (tanılama)
#[allow(deprecated)]
pub fn diagnose(dht: &Dht, keys: &Keys, role: Role) -> String {
    let key = keys.signer.verifying_key().to_bytes();
    let Some(item) = dht.get_mutable_most_recent(&key, Some(role.salt())) else {
        return "DHT'de bu anahtara ait kayıt yok (karşı taraf yayın yapmıyor ya da DHT'ye ulaşamıyor)".into();
    };
    let Some((timestamp, record)) = open_raw(keys, item.value()) else {
        return "DHT'de kayıt var ama bu anahtarla açılamadı (anahtar farklı)".into();
    };
    let current = now();
    let diff = current as i64 - timestamp as i64;
    let state = if fresh_at(timestamp, current) { "taze" } else { "BAYAT" };
    let nat = match record.window {
        Some(window) => format!("simetrik (son görülen eşleme {window})"),
        None => "koni ya da ölçülemedi (kayıtta pencere yok)".into(),
    };
    format!(
        "kayıt {state}: {} sn önce yazılmış (bizim saatimize göre), adaylar: {}, karşının NAT'ı: {nat}",
        diff,
        crate::addr::candidates_text(&record.candidates)
    )
}

/// Bu tarafın adaylarını bırakır. Birkaç saniye sürer; ayrı iş parçacığında çağrılmalı.
#[allow(deprecated)]
pub fn publish(dht: &Dht, keys: &Keys, role: Role, record: &Record) -> bool {
    let Some(value) = seal(keys, record) else {
        return false;
    };
    // Sıra numarası zaman: her yeni kayıt öncekinden büyük, eski oturumun kaydını ezer
    let seq = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let item = MutableItem::new(keys.signer.clone(), &value, seq, Some(role.salt()));
    dht.put_mutable(item, None).is_ok()
}

/// Karşı tarafın en yeni adaylarını okur; yoksa, eskiyse ya da açılamazsa `None`.
#[allow(deprecated)]
pub fn fetch(dht: &Dht, keys: &Keys, role: Role) -> Option<Record> {
    fetch_stamped(dht, keys, role).map(|(_, record)| record)
}

/// `fetch` ile aynı, kaydın yazılma zamanıyla birlikte: zaman değiştiyse karşı taraf kaydını yenilemiştir
#[allow(deprecated)]
pub fn fetch_stamped(dht: &Dht, keys: &Keys, role: Role) -> Option<(u64, Record)> {
    let key = keys.signer.verifying_key().to_bytes();
    let item = dht.get_mutable_most_recent(&key, Some(role.salt()))?;
    let (timestamp, record) = open_raw(keys, item.value())?;
    fresh_at(timestamp, now()).then_some((timestamp, record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mainline::Testnet;
    use std::net::Ipv4Addr;

    #[test]
    fn candidates_travel_through_the_dht_only_with_the_right_password() {
        let testnet = Testnet::new(5).unwrap();
        let node = || {
            Dht::builder()
                .bootstrap(&testnet.bootstrap)
                .bind_address(Ipv4Addr::LOCALHOST)
                .build()
                .unwrap()
        };
        let (host, client) = (node(), node());
        let record = Record {
            candidates: vec!["203.0.113.7:41000".parse().unwrap()],
            window: Some("203.0.113.7:26345".parse().unwrap()),
        };
        let right = keys_in(b"test\0", "ayse-pc", "dogru-parola");
        assert!(publish(&host, &right, Role::Host, &record));
        assert_eq!(
            fetch(&client, &keys_in(b"test\0", "ayse-pc", "dogru-parola"), Role::Host),
            Some(record)
        );
        // Yanlış parola ne bulur ne de okuyabilir; öbür rolün kaydı karışmaz
        assert_eq!(
            fetch(&client, &keys_in(b"test\0", "ayse-pc", "yanlis-parola"), Role::Host),
            None
        );
        assert_eq!(fetch(&client, &right, Role::Client), None);
    }

    #[test]
    fn records_expire_and_future_dates_are_bounded() {
        let current = 100_000;
        assert!(fresh_at(current - FRESH_SECS, current));
        assert!(!fresh_at(current - FRESH_SECS - 1, current));
        assert!(fresh_at(current + CLOCK_SKEW_SECS, current));
        assert!(!fresh_at(current + CLOCK_SKEW_SECS + 1, current));
        assert!(!fresh_at(u64::MAX, current));
    }

    #[test]
    fn stale_or_tampered_records_are_ignored() {
        let k = keys_in(b"test\0", "ayse-pc", "dogru-parola");
        let record = Record {
            candidates: vec!["203.0.113.7:41000".parse().unwrap()],
            window: None,
        };
        let sealed = seal(&k, &record).unwrap();
        assert!(open(&k, &sealed).is_some());
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&k, &tampered).is_none());
        assert!(open(&keys_in(b"test\0", "ayse-pc", "baska"), &sealed).is_none());
    }
}
