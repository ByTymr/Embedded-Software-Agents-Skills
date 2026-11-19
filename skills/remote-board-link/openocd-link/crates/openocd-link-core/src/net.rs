//! Telsiz protokolü: [u32 uzunluk][u8 tip][yük], little-endian.

use blake2::{Blake2s256, Digest};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ───────────── şifreli kanal ─────────────
//
// Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s: iki taraf da ortak paroladan türetilen anahtarı
// bildiğini kanıtlar (karşılıklı doğrulama), oturum anahtarı her bağlantıda yenidir
// (ileri gizlilik) ve tüm trafik ChaCha20-Poly1305 ile şifrelenir/doğrulanır.

const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const CHUNK: usize = 32 * 1024;

fn err(m: impl Into<String>) -> io::Error {
    io::Error::other(m.into())
}

/// Parola doğrulamalı anahtar anlaşması (SPAKE2): parola ağdan geçmez ve kısa/basit bir parola
/// bile dinleyen biri tarafından sonradan (çevrimdışı) denenerek bulunamaz; saldırgan ancak
/// her tahmin için yeni bir bağlantı deneyebilir (bu da `rate_ok` ile yavaşlatılır).
/// Doğru parolada iki taraf aynı anahtarı, yanlışta farklı anahtarı elde eder.
fn pake_key(s: &mut TcpStream, secret: &str, initiator: bool) -> io::Result<[u8; 32]> {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    let (st, msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(secret.as_bytes()),
        &Identity::new(b"openocd-link/1"),
    );
    let peer = if initiator {
        send_raw(s, &msg)?;
        recv_raw(s, 256)?
    } else {
        let p = recv_raw(s, 256)?;
        send_raw(s, &msg)?;
        p
    };
    let key = st
        .finish(&peer)
        .map_err(|_| err("kimlik doğrulaması başarısız"))?;
    let mut h = Blake2s256::new();
    h.update(b"openocd-link-psk-v1");
    h.update(&key);
    Ok(h.finalize().into())
}

/// Kabul eden tarafta başarısız deneme sınırı: aynı adresten 60 sn içinde en çok 5 hata.
static FAILS: Mutex<
    Option<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
> = Mutex::new(None);

fn rate_ok(ip: std::net::IpAddr) -> bool {
    let m = FAILS.lock().unwrap();
    match m.as_ref().and_then(|m| m.get(&ip)) {
        Some((n, t)) => *n < 5 || t.elapsed() > Duration::from_secs(60),
        None => true,
    }
}

fn note_result(ip: std::net::IpAddr, ok: bool) {
    let mut g = FAILS.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    if ok {
        m.remove(&ip);
    } else {
        let e = m.entry(ip).or_insert((0, std::time::Instant::now()));
        if e.1.elapsed() > Duration::from_secs(60) {
            *e = (0, std::time::Instant::now());
        }
        e.0 += 1;
        e.1 = std::time::Instant::now();
    }
}

fn send_raw(s: &mut TcpStream, b: &[u8]) -> io::Result<()> {
    let mut out = (b.len() as u16).to_le_bytes().to_vec();
    out.extend_from_slice(b);
    s.write_all(&out)
}

fn recv_raw(s: &mut TcpStream, max: usize) -> io::Result<Vec<u8>> {
    let mut l = [0u8; 2];
    s.read_exact(&mut l)?;
    let n = u16::from_le_bytes(l) as usize;
    if n > max {
        return Err(err("geçersiz kayıt boyutu"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// El sıkışma özetinden iki ekranda gösterilecek 6 haneli doğrulama kodu ("482 913").
pub fn sas(hash: &[u8; 32]) -> String {
    let mut h = Blake2s256::new();
    h.update(b"openocd-link-sas-v1");
    h.update(hash);
    let d: [u8; 32] = h.finalize().into();
    let n = u32::from_le_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Doğrulanmış + şifreli akış. Okuma ve yazma yarıları `try_clone` ile ayrı thread'lere verilebilir.
pub struct Secure {
    s: TcpStream,
    st: Arc<snow::StatelessTransportState>,
    send_n: u64,
    recv_n: u64,
    rbuf: Vec<u8>,
    rpos: usize,
    /// Her iki tarafta aynı olan el sıkışma özeti (ses anahtarı türetmek için)
    pub hash: [u8; 32],
}

/// Parolayla el sıkışır. Başarısızlık = parola yanlış ya da karşı taraf openocd-link değil.
pub fn handshake(s: TcpStream, initiator: bool, secret: &str) -> io::Result<Secure> {
    handshake_with(s, initiator, secret, true)
}

/// IP başına sınırsız el sıkışma: QUIC köprüsünde `peer_addr` hep 127.0.0.1 olduğundan sınırı
/// çağıran uygulama gerçek uzak adres üzerinden kendisi uygular.
pub fn handshake_unlimited(s: TcpStream, initiator: bool, secret: &str) -> io::Result<Secure> {
    handshake_with(s, initiator, secret, false)
}

fn handshake_with(mut s: TcpStream, initiator: bool, secret: &str, limited: bool) -> io::Result<Secure> {
    let ip = s.peer_addr().map(|a| a.ip()).ok().filter(|_| limited);
    if !initiator {
        if let Some(ip) = ip {
            if !rate_ok(ip) {
                return Err(err("çok fazla başarısız deneme, bir süre bekleyin"));
            }
        }
    }
    let r = handshake_inner(&mut s, initiator, secret);
    if !initiator {
        if let Some(ip) = ip {
            note_result(ip, r.is_ok());
        }
    }
    r.map(|(st, hash)| Secure {
        s,
        st: Arc::new(st),
        send_n: 0,
        recv_n: 0,
        rbuf: Vec::new(),
        rpos: 0,
        hash,
    })
}

fn handshake_inner(
    s: &mut TcpStream,
    initiator: bool,
    secret: &str,
) -> io::Result<(snow::StatelessTransportState, [u8; 32])> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let psk = pake_key(s, secret, initiator)?;
    let params = NOISE.parse().map_err(|e| err(format!("{e}")))?;
    let b = snow::Builder::new(params)
        .psk(0, &psk)
        .map_err(|e| err(format!("{e}")))?
        .prologue(b"openocd-link/1")
        .map_err(|e| err(format!("{e}")))?;
    let mut hs = if initiator {
        b.build_initiator()
    } else {
        b.build_responder()
    }
    .map_err(|e| err(format!("{e}")))?;
    let auth = |_| err("kimlik doğrulaması başarısız (parola yanlış olabilir)");
    let mut buf = [0u8; 1024];
    if initiator {
        let n = hs
            .write_message(&[], &mut buf)
            .map_err(|e| err(format!("{e}")))?;
        send_raw(s, &buf[..n])?;
        let m = recv_raw(s, 1024)?;
        hs.read_message(&m, &mut buf).map_err(auth)?;
    } else {
        let m = recv_raw(s, 1024)?;
        hs.read_message(&m, &mut buf).map_err(auth)?;
        let n = hs
            .write_message(&[], &mut buf)
            .map_err(|e| err(format!("{e}")))?;
        send_raw(s, &buf[..n])?;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&hs.get_handshake_hash()[..32]);
    let st = hs
        .into_stateless_transport_mode()
        .map_err(|e| err(format!("{e}")))?;
    s.set_read_timeout(None)?;
    s.set_write_timeout(None)?;
    Ok((st, hash))
}

impl Secure {
    pub fn try_clone(&self) -> io::Result<Secure> {
        Ok(Secure {
            s: self.s.try_clone()?,
            st: self.st.clone(),
            send_n: self.send_n,
            recv_n: self.recv_n,
            rbuf: Vec::new(),
            rpos: 0,
            hash: self.hash,
        })
    }
    /// Okuyucuyu uyandırmak için iki yönü de kapatır
    pub fn shutdown(&self) {
        let _ = self.s.shutdown(std::net::Shutdown::Both);
    }
    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.s.set_read_timeout(d)
    }
    pub fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.s.set_write_timeout(d)
    }
}

impl Read for Secure {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.rpos >= self.rbuf.len() {
            let ct = recv_raw(&mut self.s, CHUNK + 64)?;
            let mut pt = vec![0u8; ct.len()];
            let n = self
                .st
                .read_message(self.recv_n, &ct, &mut pt)
                .map_err(|_| err("şifre çözülemedi: kayıt doğrulanamadı"))?;
            self.recv_n += 1;
            pt.truncate(n);
            self.rbuf = pt;
            self.rpos = 0;
        }
        let n = out.len().min(self.rbuf.len() - self.rpos);
        out[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        Ok(n)
    }
}

impl Write for Secure {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = data.len().min(CHUNK);
        let mut ct = vec![0u8; n + 64];
        let l = self
            .st
            .write_message(self.send_n, &data[..n], &mut ct)
            .map_err(|e| err(format!("şifrelenemedi: {e}")))?;
        self.send_n += 1;
        send_raw(&mut self.s, &ct[..l])?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn pair(a: &'static str, b: &'static str) -> (io::Result<Secure>, io::Result<Secure>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = thread::spawn(move || handshake(l.accept().unwrap().0, false, b));
        let cli = handshake(TcpStream::connect(addr).unwrap(), true, a);
        (cli, srv.join().unwrap())
    }

    #[test]
    fn dogru_parola_sifreli_kanal() {
        let (c, s) = pair("ABCD-EFGH-JKLM-NPQR", "ABCD-EFGH-JKLM-NPQR");
        let (mut c, mut s) = (c.unwrap(), s.unwrap());
        assert_eq!(c.hash, s.hash, "iki taraf aynı oturum özetini görmeli");
        c.write_all(b"merhaba").unwrap();
        let mut small = [0u8; 7];
        s.read_exact(&mut small).unwrap();
        assert_eq!(&small, b"merhaba");
        // büyük veri (parçalama)
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let big2 = big.clone();
        let w = thread::spawn(move || {
            c.write_all(&big2).unwrap();
        });
        let mut got = vec![0u8; big.len()];
        s.read_exact(&mut got).unwrap();
        w.join().unwrap();
        assert_eq!(got, big);
    }

    #[test]
    fn kisa_parola_da_calisir() {
        let (c, s) = pair("1234", "1234");
        assert!(c.is_ok() && s.is_ok(), "4 haneli kod ile kanal kurulmalı");
        let (c, s) = pair("482913", "482914");
        assert!(s.is_err() || c.is_err(), "bir hane farklı kod reddedilmeli");
    }

    #[test]
    fn art_arda_yanlis_denemeler_engellenir() {
        let ip: std::net::IpAddr = "10.9.9.9".parse().unwrap();
        for _ in 0..5 {
            assert!(rate_ok(ip));
            note_result(ip, false);
        }
        assert!(!rate_ok(ip), "5 başarısız denemeden sonra engellenmeli");
        note_result(ip, true);
        assert!(rate_ok(ip), "başarılı bağlantı sayacı sıfırlamalı");
    }

    #[test]
    fn yanlis_parola_reddedilir() {
        let (c, s) = pair("ABCD-EFGH-JKLM-NPQR", "XXXX-EFGH-JKLM-NPQR");
        assert!(
            c.is_err() || s.is_err(),
            "farklı parolalarla kanal kurulmamalı"
        );
        assert!(s.is_err(), "kabul eden taraf yanlış parolayı reddetmeli");
    }

    #[test]
    fn bozulan_kayit_reddedilir() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let secret = "ABCD-EFGH-JKLM-NPQR";
        // araya giren: iki taraf arasında baytları ileten ve birini bozan vekil
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let paddr = proxy.local_addr().unwrap();
        let srv = thread::spawn(move || handshake(l.accept().unwrap().0, false, secret).unwrap());
        thread::spawn(move || {
            let (mut a, _) = proxy.accept().unwrap();
            let mut b = TcpStream::connect(addr).unwrap();
            let (mut a2, mut b2) = (a.try_clone().unwrap(), b.try_clone().unwrap());
            thread::spawn(move || {
                io::copy(&mut b2, &mut a2).ok();
            });
            let mut buf = [0u8; 4096];
            let mut count = 0;
            while let Ok(n) = a.read(&mut buf) {
                if n == 0 {
                    break;
                }
                count += 1;
                if count > 2 {
                    buf[n - 1] ^= 0xFF; // SPAKE2 + Noise el sıkışmasından sonraki ilk veri kaydını boz
                }
                if b.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        });
        let mut c = handshake(TcpStream::connect(paddr).unwrap(), true, secret).unwrap();
        let mut s = srv.join().unwrap();
        c.write_all(b"merhaba").unwrap();
        assert!(
            s.read_exact(&mut [0u8; 7]).is_err(),
            "bozulmuş kayıt kabul edilmemeli"
        );
    }
}
