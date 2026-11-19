> **Implementation note (2026-11-01).** This plan was implemented as **openocd-link** (not
> neyber-link). Differences from the text below: the code lives in `../openocd-link/` as a
> standalone workspace (`openocd-link-core` + `openocd-link`), not inside Ulac; the key prefix is
> `OL1-`, the handshake magic `OCDLNK01`, and every derivation context uses `openocd-link/...`, so
> it is not wire-compatible with neyber. Phases 1-8 are done; phase 9 (the Python `eat remote`
> wrapper) exists only in the standalone EmbeddedAiTool repo; phase 10 (field test over two
> networks with a real board) is not done. The Raspberry Pi package installs the service so that it
> starts once at install and is **not** enabled at boot.

# neyber-link: Uzak kart servisi — uygulama planı

Tarih: 2026-11-01 · Durum: plan (henüz kod yok)

Bu belge, başka ağdaki bir bilgisayara takılı geliştirme kartına (ST-Link + OpenOCD) internet
üzerinden, sunucusuz ve uçtan uca şifreli erişim sağlayan **neyber-link** aracının ve onun
**EmbeddedAiTool** entegrasyonunun eksiksiz uygulama planıdır. Belge, bunu uygulayacak bir
yapay zekâ ajanının başka kaynağa bakmadan, tahmin yürütmeden ilerleyebileceği ayrıntıda yazıldı.

---

## 0. Bu planı uygulayacak ajan için kurallar

1. **Fazları sırayla uygula** (§12). Bir fazın kabul ölçütleri geçmeden sonrakine başlama.
   Her fazın sonunda çalıştırdığın komutları ve çıktılarını raporla.
2. **Ulac/neyber masaüstü uygulamasının davranışını değiştirme.** Özellikle şunlar bayt bayt
   aynı kalır: `rendezvous::keys`, `net::handshake`, `direct.rs` içindeki `MAGIC`, oturum
   protokolü. Eski ve yeni sürüm neyber birbirine bağlanabilmeye devam etmeli.
3. **`tunnel.rs` dosyasının içini değiştirme**; yalnız taşı. neyber-link onu olduğu gibi kullanır (§7.7).
4. **Bu belgede tanımlanmayan protokol alanı, kanal, komut veya yapılandırma anahtarı ekleme.**
   Belge bir konuda sessizse ya da çelişkiliyse dur ve kullanıcıya sor.
5. **Gizli anahtar hiçbir yere yazılmaz:** log, argv, ortam değişkeni, durum dosyası, JSON
   çıktısı, hata mesajı, test fikstürü, commit. Görünmesi gereken her yerde yalnız
   **parmak izi** (§7.1) kullanılır. Tek istisna: `keygen` anahtarı bir kez stdout'a yazar.
6. **Kod yorumları ve kullanıcı mesajları Türkçe**, mevcut dosyaların üslubunda: kısa `///`
   ve `//` açıklamaları, gereksiz yorum yok. Rust edition 2024.
7. **Commit ancak kullanıcı isterse atılır.** Her faz için ayrı commit önerilir.
8. **Komut yazım kuralları** `~/.claude/CLAUDE.md` dosyasındadır: `$HOME` ve
   `$(...)` yok, `&&` ve `;` ile zincir yok, mutlak yol kullan, git için `git -C <yol>`.
9. **Birim testler internete çıkmaz.** DHT ve STUN testlerde kapalıdır. İnternet gerektiren
   testler elle çalıştırılır ve `NEYBER_LINK_LIVE=1` ile açılır.
10. **Bağımlılık eklerken sürüm uydurma.** `cargo add <crate>` ile ekle, çözülen sürümü kullan.
    Bu belgede sürümü yazan satırlar mevcut `Cargo.toml` dosyasından kopyadır.

---

## 1. Amaç

Uzaktaki cihaza bir kez **servis** kurulur. O cihazın **bağlantı anahtarını** bilen herkes,
kendi bilgisayarından şu komutu verir:

```
neyber-link connect --key-file lab.key --session lab
```

Birkaç saniye içinde şunlar kullanılabilir hâle gelir:

| Yetenek | Nasıl |
|---|---|
| OpenOCD Tcl RPC ve GDB | Yerel `127.0.0.1:6666` ve `127.0.0.1:3333`, uzaktaki OpenOCD'ye tünellenir. `eat` ve `gdb` hiçbir şey değişmemiş gibi çalışır. |
| OpenOCD'yi yönetmek | `neyber-link openocd start/stop/status/log`. OpenOCD'yi uzaktaki servis, kendi yapılandırmasındaki argümanlarla başlatır. |
| İki yönlü dosya aktarımı | `neyber-link put fw.axf` ve `neyber-link get fw.axf`. Uzaktaki dosyalar tek bir çalışma klasöründe tutulur. |
| Komut çalıştırmak | `neyber-link exec -- make -C proje` çıktıyı ve çıkış kodunu verir. Yapay zekâ ajanları için uygundur. |
| Etkileşimli terminal | `neyber-link shell` uzaktaki cihazda tam bir PTY kabuğu açar; içinde kod düzenlenebilir ve derlenebilir. |

Erişim kuralı: **anahtarı bilen bağlanır.** Uzak cihazda onay penceresi çıkmaz.

## 2. Kapsam dışı

- **Relay sunucusu:** her iki taraf da simetrik NAT veya CGNAT arkasındaysa bağlantı kurulamayabilir (§14).
- **Aynı anda birden çok istemci:** servis tek oturum kabul eder.
- **Web arayüzü ya da GUI.**
- **Windows ve macOS'ta servis olarak çalışma.** İstemci bu sistemlerde çalışır; servis ilk sürümde yalnız Linux/systemd destekler.
- **Kişi başına ayrı anahtar, süreli anahtar ve tek tek iptal.** Gelecek iş olarak §15'te listelendi.

## 3. Kararlar

Kullanıcı aksini söylemedikçe bu varsayılanlar geçerlidir.

| Konu | Karar |
|---|---|
| Kodun yeri | Ulac reposu (`<ulac reposu>`) Cargo workspace'e çevrilir. Yeni crate'ler: `crates/neyber-core` (ağ çekirdeği) ve `crates/neyber-link` (CLI ve servis). |
| Uzak cihaz | Linux + systemd (Raspberry Pi OS, Debian, Ubuntu) |
| Anahtar | Servis üretir, 128 bit rastgele, sabittir. `keygen --force` ile yenilenir ve eskisi hemen geçersiz olur. |
| Oturum | Aynı anda tek istemci. İkinci gelen `busy` yanıtı alır. |
| Varsayılan açık yetenekler | Port tünelleri (tcl 6666, gdb 3333), dosya aktarımı, OpenOCD yönetimi |
| Varsayılan kapalı yetenekler | `exec` ve `shell`. Uzak cihazın yöneticisi yapılandırmada açıkça açar (§8.2). |
| OpenOCD telnet portu (4444) | Her zaman kapalı (`telnet_port disabled`) |
| Dosya klasörü | `/var/lib/neyber-link/files` (düz klasör, alt klasör yok) |
| İstemci platformu | Linux birincil hedef. Windows ve macOS'ta da derlenecek şekilde yazılır. |

---

## 4. Mimari

```
┌──────────────────────── Senin bilgisayarın ────────────────────────┐      ┌───────────── Uzak cihaz (kart takılı) ─────────────┐
│                                                                    │      │                                                    │
│  eat ... / arm-none-eabi-gdb        neyber-link put|get|exec|shell │      │   neyber-link serve  (systemd, sürekli açık)       │
│      │  TCP                                │ yerel kontrol soketi  │      │      │        │         │          │                 │
│      ▼                                     ▼ (127.0.0.1, jeton)    │      │      │        │         │          │                 │
│  127.0.0.1:6666 / :3333 ◄──────── neyber-link connect (daemon)     │      │   Tunnel   dosya    OpenOCD     exec /            │
│                                     │                              │      │   kanalları klasörü  süreci      PTY kabuk         │
│                                     │ tek şifreli oturum           │      │      │                 ▲                             │
│                                     └══ QUIC/UDP (NAT delme) ══════╪══════╪══════┘                 │ 127.0.0.1:6666/3333         │
│                                                                    │      │                    ST-Link ── kart                 │
└────────────────────────────────────────────────────────────────────┘      └────────────────────────────────────────────────────┘
                     buluşma: BitTorrent DHT (imzalı ve şifreli kayıt) + herkese açık STUN
```

Akış:

1. Servis kendi aday adreslerini anahtardan türeyen DHT kaydına yazar ve istemci kaydını izler.
2. İstemci kendi kaydını yazar, servisin kaydını okur ve QUIC ile bağlanır. Servis de istemcinin adreslerine delik açar.
3. QUIC akışı yerelde bir TCP soketine köprülenir. Bu `punch.rs`'nin mevcut davranışıdır; sonuç `Bridged = (TcpStream, SocketAddr)`.
4. Bu soket üzerinde `NEYLNK01` imzası gönderilir, ardından SPAKE2 + Noise el sıkışması yapılır (`net::handshake`) ve şifreli bir `Secure` akışı elde edilir.
5. Taraflar tanıtım mesajlarını (hello) değişir. Sonrasında tek akış üzerinde çerçeveli kanallar çalışır: kontrol, tüneller, dosya, exec çıktısı, kabuk, ping.

---

## 5. Mevcut kod envanteri

Bunlar yeniden yazılmaz, kullanılır. İmzalar 2026-11-01 tarihli çalışma ağacından alındı.

| Dosya (şu an) | Kullanılacak öğeler |
|---|---|
| `src/punch.rs` | `pub fn start(port: u16, own: Option<Vec<IpAddr>>) -> Arc<Node>`, `Node::candidates() -> Option<Vec<SocketAddr>>`, `Node::punch(Vec<SocketAddr>)`, `Node::punch_for(Vec<SocketAddr>, Duration)`, `Node::accept(Option<SyncSender<Bridged>>)`, `Node::dial(Vec<SocketAddr>, SyncSender<io::Result<Bridged>>, Arc<AtomicBool>)`, `pub fn window(SocketAddr) -> Vec<SocketAddr>`, `pub type Bridged = (TcpStream, SocketAddr)` |
| `src/stun.rs` | `pub fn probe_nat() -> Option<(SocketAddr, bool)>` (bool = simetrik NAT) |
| `src/rendezvous.rs` | `shared() -> Option<&'static Dht>`, `keys(&str, &str) -> Keys`, `Record { candidates, window }`, `publish(&Dht, &Keys, Role, &Record) -> bool`, `fetch(&Dht, &Keys, Role) -> Option<Record>`, `Role::{Host, Client}`. Kayıt 15 dakika tazedir (`FRESH_SECS`). |
| `src/net.rs` | `handshake(TcpStream, initiator: bool, secret: &str) -> io::Result<Secure>`, `Secure: Read + Write`, `Secure::try_clone`, `set_read_timeout`, `set_write_timeout`, `sas(&[u8; 32]) -> String`, `Secure::hash` |
| `src/tunnel.rs` | `Tunnel::new() -> Arc<Tunnel>`, alanlar `allowed_port`, `local_port`, `remote_port: AtomicU16`, `status: Mutex<String>`; `run(self: &Arc<Self>, host: bool, alive, send)`, `receive(&[u8]) -> io::Result<()>`, `close()`, `pub const MAX_PACKET: usize = 8197` |
| `src/portmap.rs` | İlk sürümde kullanılmaz; yalnız çekirdeğe taşınır. |
| `src/direct.rs` | Örnek alınacak akışlar: `meet()` (DHT döngüsü), `authenticate()` + `Deadline` (imza + el sıkışma + 12 sn üst sınır), `probe_window()`, `host_stream()` (tek yazıcı + klon okuyucu deseni) |

Bu modüllerin projenin geri kalanına bağlı olduğu yerler:

- `punch.rs`: `crate::direct::log`, `crate::direct::candidates_text`, `crate::direct::show_addr`, `crate::stun`
- `rendezvous.rs`: `crate::direct::candidates_text`, `crate::direct::parse_candidates`
- `net.rs`, `stun.rs`, `portmap.rs`, `tunnel.rs`: proje içi bağımlılık yok

Modüllerin kullandığı dış crate'ler:

| Modül | Crate'ler |
|---|---|
| `punch` | quinn (rustls), rcgen, socket2, tokio |
| `rendezvous` | blake2, chacha20poly1305, getrandom, mainline |
| `net` | blake2, snow, spake2 |
| `stun` | getrandom |

---

## 6. Hedef dizin yapısı

```
ulac/
├── Cargo.toml                     # [package] neyber + [workspace]
├── src/                           # neyber masaüstü uygulaması (net/punch/... buradan çıkar)
├── crates/
│   ├── neyber-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── addr.rs            # candidates_text, parse_list, show_addr
│   │       ├── log.rs             # set_logger / log
│   │       ├── net.rs  punch.rs  portmap.rs  rendezvous.rs  stun.rs  tunnel.rs   (taşındı)
│   └── neyber-link/
│       ├── Cargo.toml
│       ├── src/
│       │   ├── main.rs            # CLI (clap)
│       │   ├── lib.rs
│       │   ├── key.rs             # anahtar biçimi, türetmeler
│       │   ├── config.rs          # servis yapılandırması (TOML)
│       │   ├── wire.rs            # çerçeve, kanal sabitleri, hello
│       │   ├── meet.rs            # DHT buluşma döngüsü
│       │   ├── auth.rs            # imza + el sıkışma + süre sınırı + deneme sınırı
│       │   ├── server/            # mod.rs, session.rs, files.rs, openocd.rs, exec.rs, shell.rs
│       │   ├── client/            # mod.rs, session.rs, control.rs (yerel soket), state.rs
│       │   └── output.rs          # insan metni / --json
│       └── tests/
│           └── loopback.rs        # aynı makinede servis + istemci uçtan uca
├── packaging/linux/
│   ├── neyber-link.service
│   ├── neyber-link.toml           # örnek yapılandırma
│   └── install-link.sh
└── docs/UZAK_KART_SERVISI_PLANI.md   (bu belge)
```

---

## 7. Protokol

### 7.1 Bağlantı anahtarı

**Görünüm:** `NL1-` ön eki ve dörderli 8 grupta 32 karakter. Örnek: `NL1-000G-40R4-0M30-E209-185G-R38E-1YK7-PB6P`

**İçerik:** 20 bayt = 16 bayt rastgele sır + 4 bayt sağlama.

- `sır = getrandom(16 bayt)`
- `sağlama = Blake2s256(b"neyber-link/key-check/v1" ‖ sır)[0..4]`
- Kodlama: Crockford base32, alfabe `0123456789ABCDEFGHJKMNPQRSTVWXYZ`. 160 bit en anlamlı bitten başlayarak 5'er bit okunur ve 32 karakter elde edilir.

**Okuma kuralları** (sırayla uygulanır):

1. Boşlukları ve `-` karakterlerini sil, büyük harfe çevir.
2. Metin `NL1` ile başlamıyorsa hata: "Anahtar NL1- ile başlamalı".
3. Önekten sonra tam 32 karakter kalmalı. Değilse hata: "Anahtar uzunluğu yanlış".
4. Karıştırılan harfleri dönüştür: `O → 0`, `I → 1`, `L → 1`. Alfabe dışında karakter varsa hata.
5. Kodu çöz ve sağlamayı doğrula. Tutmuyorsa hata: "Anahtar yanlış yazılmış (sağlama tutmadı)".

Bu kontrol ağa çıkmadan yapılır. Yazım hatası servisin deneme sınırına takılmaz.

**Türetmeler** (hepsi Blake2s256; `‖` birleştirme demek):

| Ad | Tanım | Kullanım |
|---|---|---|
| DHT anahtarları | `rendezvous::keys_in(b"neyber-link/rendezvous/v1\0", "link", hex(sır))` | `publish` / `fetch` |
| PAKE sırrı | `hex(Blake2s256(b"neyber-link/pake/v1\0" ‖ sır))`, 64 karakter | `net::handshake(.., secret)` |
| Parmak izi | `hex(Blake2s256(b"neyber-link/fingerprint/v1\0" ‖ sır))[0..8]` | Log, durum dosyası ve çıktılarda anahtarın yerine |

`hex` küçük harflidir. `keys_in` Faz 1'de eklenir (§12, Faz 1, adım 8).

**Test vektörü** (`key.rs` testlerinde birebir kullanılır):

```
sır          = 000102030405060708090a0b0c0d0e0f
sağlama      = a67b2cd6
anahtar      = NL1-000G-40R4-0M30-E209-185G-R38E-1YK7-PB6P
PAKE sırrı   = cb5de5ad4204dc04d7a27d20098ec1de02f840b81b9c6bb17647db32c11b1d65
parmak izi   = 1a2e2e79
DHT parolası = 000102030405060708090a0b0c0d0e0f   (kullanıcı adı "link")
```

### 7.2 Buluşma (DHT)

`direct.rs` içindeki `meet()` fonksiyonunun kopyası `meet.rs` dosyasına yazılır. Farklar:

| | Servis (`Role::Host`) | İstemci (`Role::Client`) |
|---|---|---|
| Kendi kaydını yaz | Aday listesi değişince ya da 300 sn'de bir | Değişince ya da 60 sn'de bir |
| Karşı kaydı oku | `poll_secs` aralıkla (varsayılan 4 sn) | 3 sn'de bir |
| Yeni karşı aday | `node.punch(new)` | Önce `probe_window()`, ardından `node.dial(new, tx, cancel)` |
| Karşının `window` alanı | `node.punch_for(punch::window(start), 120 sn)`, her yeni `start` için bir kez | Yok |
| Durma | Hiç durmaz. Oturum bitince `reset()` denenmiş listeyi temizler, böylece aynı istemci yeniden bağlanabilir. | Bağlantı kurulunca ya da iptal edilince |

- Testlerde ve `--no-dht` bayrağıyla DHT döngüsü hiç başlatılmaz.
- `rendezvous::shared()` `None` dönerse bir kez log yazılır: "DHT'ye katılınamadı". Servis yine de `--peer` ile gelen bağlantılara açık kalır.

### 7.3 Bağlantı kurma ve kimlik doğrulama (`auth.rs`)

`direct.rs` içindeki `authenticate()` ve `Deadline` kopyalanır. Tek fark imzadır: `const MAGIC: &[u8; 8] = b"NEYLNK01";`

1. `socket.set_nodelay(true)`, okuma ve yazma zaman aşımı 5 sn. Bütün aşama için `Deadline` ile 12 sn üst sınır konur.
2. İstemci `MAGIC` yazar; servis 8 bayt okur ve karşılaştırır. Uymazsa sessizce kapatır. Bu durum deneme sayılmaz, log'a yazılmaz.
3. `net::handshake(socket, initiator, &key.pake_secret())`
4. Hata mesajı istemcide şöyle çevrilir: `UnexpectedEof` ya da `ConnectionReset` → "Servis bağlantıyı kabul etmedi: anahtar yanlış ya da sürümler uyumsuz" (çıkış kodu 5).

**Deneme sınırı (yalnız servis).** QUIC köprüsünden gelen soketin `peer_addr()` değeri `127.0.0.1` olur. Bu yüzden `net.rs`'nin IP başına sınırı tek bir kova gibi davranır. Servis kendi sınırını `Bridged.1` (gerçek uzak adres) üzerinden uygular:

- Aynı IP 60 sn içinde 5 başarısız el sıkışma yaptıysa, o IP'den gelen bağlantılar el sıkışmasız kapatılır. Süre son başarısızlıktan itibaren 60 sn'dir.
- Tüm IP'lerden 60 sn içinde 30 başarısızlık olursa, yeni bağlantı kabulü 60 sn durdurulur ve log'a yazılır.
- Servis bu nedenlerle **asla kapanmaz**. `direct.rs`'deki "5 başarısızlıkta dur" davranışı kopyalanmaz.
- Aynı anda en fazla 4 el sıkışma sürebilir; fazlası hemen kapatılır.

### 7.4 Tanıtım (hello)

El sıkışmadan hemen sonra, çerçevelemeden önce iki mesaj gider. Biçim: `u16 BE uzunluk` + UTF-8 JSON, en fazla 4096 bayt.

**İstemci → servis:**

```json
{"proto": 1, "name": "my-laptop"}
```

`name` alanı yalnız log içindir: en fazla 64 karakter, `[A-Za-z0-9._-]` dışındaki karakterler `-` olur.

**Servis → istemci:**

```json
{
  "proto": 1,
  "status": "ok",
  "device": "lab-pi",
  "services": [{"name": "tcl", "port": 6666}, {"name": "gdb", "port": 3333}],
  "features": {"files": true, "openocd": true, "exec": false, "shell": false},
  "files_dir": "/var/lib/neyber-link/files"
}
```

`status` değerleri:

- `ok`
- `busy`: başka oturum açık. Servis yalnız `proto` ve `status` gönderip kapatır. İstemci çıkış kodu 6.
- `unsupported`: `proto` farklı. İstemci çıkış kodu 7.

Kurallar:

- `services` dizisinin sırası kanal numarasını belirler: ilk servis `0x01` kanalı, ikincisi `0x02`, en fazla 8 servis.
- Hello'dan sonra okuma zaman aşımı 15 sn, yazma zaman aşımı 10 sn olarak ayarlanır.

### 7.5 Çerçeve ve kanallar (`wire.rs`)

```
┌───────────────┬──────────┬──────────────────────────┐
│ u16 BE  len   │ u8 kanal │ yük (len − 1 bayt)       │
└───────────────┴──────────┴──────────────────────────┘
1 ≤ len ≤ 60001  (yük en fazla 60000 bayt)
```

- `write_frame` başlığı ve yükü tek bir tampona koyar ve **tek** `write_all` çağrısıyla yazar.
- Oturum başına **tek yazıcı** vardır: `Arc<Mutex<Secure>>`. Okuma, `try_clone()` ile alınan ayrı bir kopyadan yapılır ve bu kopyaya **asla yazılmaz**. İki yazıcı aynı Noise nonce'unu kullanır ve oturum bozulur. Bu, `direct.rs::host_stream` içindeki desenin aynısıdır.
- Bilinmeyen kanal ya da sınır dışı uzunluk protokol hatasıdır ve oturum kapatılır.

| Kanal | Ad | Yük |
|---|---|---|
| `0x00` | CONTROL | UTF-8 JSON (§7.6), en fazla 60000 bayt |
| `0x01`–`0x08` | TUNNEL n | `tunnel.rs` paketi olduğu gibi. `Tunnel::receive` boyutu doğrular (≤ 8197). |
| `0x10` | FILE | `u32 BE aktarım no` + veri (en fazla 8192 bayt) |
| `0x20` | SHELL | `u8 tür` + veri. Tür 1 = DATA (≤ 8192 bayt), 2 = RESIZE (`u16 BE sütun`, `u16 BE satır`), 3 = EOF (veri yok) |
| `0x30` | EXEC_OUT | `u32 BE istek no` + `u8 akış` (1 = stdout, 2 = stderr) + veri (≤ 8192 bayt) |
| `0xFE` | PING | Boş. İki taraf da 3 sn'de bir gönderir. Gelen PING'e yanıt verilmez; varlığı yeterlidir. |

### 7.6 Kontrol mesajları (kanal 0x00)

Yalnız **istemci istek gönderir**; servis yanıt verir ve olay bildirir.

- **İstek:** `{"id": <u32, sıfırdan büyük, oturumda tekil>, "op": "<ad>", ...}`
- **Başarılı yanıt:** `{"id": <aynı>, "ok": true, ...}`
- **Hatalı yanıt:** `{"id": <aynı>, "ok": false, "code": "<kod>", "error": "<Türkçe açıklama>"}`
- **Olay:** `{"event": "<ad>", ...}` (`id` yok)

Hata kodları: `bad_request`, `disabled` (yetenek kapalı), `not_found`, `exists`, `too_large`,
`no_space`, `busy`, `checksum`, `io`, `timeout`, `already_running`, `not_running`, `start_failed`.

Servis her isteği ayrı bir iş parçacığında işler; aynı anda en fazla 4 istek (fazlası `busy`).
FILE kanalındaki veri ise okuma döngüsünde doğrudan dosyaya yazılır.

| op | İstek alanları | Başarılı yanıt alanları | Açıklama |
|---|---|---|---|
| `info` | — | `device`, `services`, `features`, `files_dir`, `openocd: {configured, running}` | |
| `openocd.start` | — | `pid` | §8.4. Tcl portu 10 sn içinde TCP kabul etmezse süreç durdurulur, `start_failed` döner ve hata mesajına son 20 log satırı eklenir. |
| `openocd.stop` | — | `exit_code` (yoksa `null`) | Önce SIGTERM gönderilir; 5 sn içinde kapanmazsa SIGKILL |
| `openocd.status` | — | `running`, `pid`, `exit_code`, `since` (RFC 3339) | |
| `openocd.log` | `lines` (1–500, varsayılan 100) | `lines: [..]`, `truncated` | Yanıt 50000 baytı geçmeyecek kadar son satır |
| `file.list` | — | `files: [{name, size, mtime}]`, `truncated` | En fazla 500 kayıt. `.part` dosyaları listelenmez. |
| `file.put` | `name`, `size`, `sha256`, `transfer` | `path` (uzaktaki mutlak yol) | §7.8 |
| `file.put_end` | `transfer` | `path`, `size`, `sha256` | §7.8 |
| `file.get` | `name`, `transfer` | `size`, `sha256` | Ardından FILE çerçeveleri ve `file.end` olayı gelir |
| `file.delete` | `name` | — | |
| `exec` | `argv` (1–64 öğe), `cwd` (isteğe bağlı, dosya klasörüne göre göreli), `timeout_s` (1–`exec.timeout_max_s`, varsayılan 60) | `exit_code` (sinyalle bittiyse `null`), `signal`, `timed_out`, `truncated` | §7.9. Yanıt, tüm EXEC_OUT çerçevelerinden sonra gönderilir. |
| `shell.open` | `cols`, `rows` (1–1000), `term` (en fazla 32 karakter, varsayılan `xterm-256color`) | — | §7.10. Aynı anda tek kabuk. |
| `shell.close` | — | — | |

**Olaylar** (servis → istemci):

- `{"event":"file.end","transfer":n}`
- `{"event":"file.abort","transfer":n,"error":"..."}`
- `{"event":"shell.exit","code":n|null}`
- `{"event":"openocd.exit","code":n|null}`

### 7.7 Port tünelleri

Her `services[i]` için **ayrı bir `Tunnel`** örneği çalışır ve kanal `0x01 + i` kullanılır.
`tunnel.rs` değişmez. Bu sayede tek portlu tünel kodu olduğu gibi çok porta genişler.

**Servis tarafı, her servis için:**

1. `let t = Tunnel::new();`
2. `t.allowed_port.store(port)`. Bu, **`run` çağrısından önce** yapılmalı; ilk turda karşıya GRANT gider.
3. `t.run(true, alive, send)`. `send` paketi `write_frame(writer, 0x01 + i, paket)` ile yazar.

**İstemci tarafı, her servis için:**

1. `Tunnel::new()`
2. `run(false, alive, send)`
3. Önce `remote_port.store(servis.port)`, sonra `local_port.store(yerel_port)`. Sıra önemli: yerel bağlantı kabul edildiği anda `remote_port` okunur.
4. En fazla 1 sn boyunca 10 ms aralıkla kontrol et:
   - `local_port` değeri 0'a döndüyse yerel port açılamamıştır: çıkış kodu 8, mesaj `status` metni.
   - `status` metni `"dinleniyor"` içeriyorsa tamamdır.

**Ortak kurallar:**

- Yerel port varsayılan olarak servis portuyla aynıdır. `--map tcl=16666` ile değiştirilir.
- Bilinmeyen servis adı ya da aynı yerel portun iki kez kullanılması, bağlanmadan önce hata verir (çıkış kodu 2).
- Okuma döngüsü TUNNEL kanalından gelen yükü `tunnels[kanal - 1].receive(yük)?` ile iletir. Hata dönerse (kuyruk dolu ya da geçersiz paket) oturum kapanır.
- Oturum bitince her tünele `close()` çağrılır ve `alive` false olur. Tünelin dinleyicisi kendiliğinden kapanır.
- Her tünel en fazla 16 eşzamanlı TCP bağlantı taşır (`tunnel.rs::MAX_CONNECTIONS`).

### 7.8 Dosya aktarımı

**Dosya adı kuralı:** `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$` ve `.part` ile bitmez. Yol ayırıcısı
olmadığı için klasör dışına çıkılamaz. Kurala uymayan ad `bad_request` hatası alır.

**Gönderme (put):**

1. İstemci yerel dosyanın boyutunu ve SHA-256 değerini hesaplar, `file.put` gönderir.
2. Servis şunları kontrol eder:
   - özellik açık mı (`disabled`)
   - ad geçerli mi
   - `size ≤ max_file_mb` mi (`too_large`)
   - klasördeki toplam boyut + `size ≤ max_total_mb` mi (`no_space`)
   - aynı anda başka aktarım var mı (`busy`). Servis aynı anda tek aktarım kabul eder.

   Sorun yoksa `<ad>.part` dosyasını `create` + `truncate` ile 0600 izinle açar ve yanıt verir.
3. İstemci FILE çerçevelerini gönderir: 8192 baytlık parçalar, her çerçevede ayrı mutex kilidi. Böylece tünel trafiği arada akmaya devam eder.
4. İstemci `file.put_end` gönderir. Servis boyutu ve SHA-256 değerini doğrular.
   - Tutarsa `fsync` yapar, `<ad>.part` dosyasını `<ad>` olarak yeniden adlandırır (varsa üzerine yazar) ve yanıt verir.
   - Tutmazsa `.part` dosyasını siler ve `checksum` hatası döner.
5. Beklenen boyuttan fazla veri gelirse servis `file.abort` olayı gönderir ve `.part` dosyasını siler.
6. Oturum koparsa yarım kalan `.part` silinir. Servis açılışta da klasördeki tüm `.part` dosyalarını siler.

**Alma (get):**

1. İstemci `file.get` gönderir. Servis boyutu ve SHA-256 değerini hesaplayıp yanıt verir.
2. Servis FILE çerçevelerini gönderir, ardından `file.end` olayını gönderir.
3. İstemci veriyi `<çıktı>.part` dosyasına yazar. Boyut ve SHA-256 tutarsa yeniden adlandırır; tutmazsa siler ve çıkış kodu 1 ile döner.
4. Hedef dosya zaten varsa ve `--force` verilmemişse istemci istek göndermeden hata verir.

### 7.9 Komut çalıştırma (exec)

- `std::process::Command::new(argv[0]).args(&argv[1..])` ile çalışır. **Kabuk kullanılmaz.** Kabuk isteyen kişi `["sh", "-c", "..."]` gönderir; bu zaten açıkça izin verilmiş bir yetenektir.
- `cwd` dosya klasörüne göre göreli yoldur. Kanonik hâli klasörün dışına çıkıyorsa `bad_request`. Verilmezse çalışma klasörü dosya klasörüdür.
- stdin `/dev/null`'a bağlanır. stdout ve stderr ayrı iş parçacıklarında okunur ve EXEC_OUT çerçeveleriyle **canlı** gönderilir.
- Her akış için 1 MiB gönderilir; fazlası okunup atılır ve yanıtta `truncated: true` olur.
- Süre dolarsa süreç grubu öldürülür (Unix'te `process_group(0)` ile başlatılıp `kill(-pgid, SIGKILL)`) ve yanıtta `timed_out: true` olur.
- Aynı anda en fazla 2 exec çalışır; fazlası `busy`.

### 7.10 Etkileşimli kabuk

- Servis `portable-pty` ile PTY açar. Program ve argümanlar yapılandırmadan gelir (§8.2). Ortama `TERM` eklenir, çalışma klasörü kullanıcının ev dizinidir.
- PTY çıktısı SHELL/DATA çerçeveleriyle gelir; istemcinin SHELL/DATA çerçeveleri PTY girişine yazılır. RESIZE çerçevesi PTY boyutunu değiştirir.
- Kabuk çıkınca servis `shell.exit` olayını gönderir.
- İstemcinin `neyber-link shell` komutu `crossterm` ile terminali ham moda alır. Çıkışta ham mod **her durumda** geri alınır: `Drop` korumasıyla, panik dahil.
- Pencere boyutu ilk açılışta gönderilir. Sonraki değişiklikler Unix'te SIGWINCH ile, `signal-hook` crate'i kullanılarak yakalanır.

### 7.11 Canlılık ve zaman aşımları

| Olay | Süre |
|---|---|
| Aday toplama (STUN), istemci | `node.candidates()` en fazla 10 sn beklenir |
| Karşıyı bulma ve yol açma, istemci | `--timeout` (varsayılan 60 sn), aşılırsa çıkış kodu 4 |
| İmza + el sıkışma | 12 sn (`Deadline`) |
| Hello sonrası okuma | 15 sn; bu sürede hiç çerçeve gelmezse oturum ölü sayılır |
| PING aralığı | 3 sn |
| Yazma | 10 sn |
| `openocd.start` hazır olma | 10 sn |
| `openocd.stop` | 5 sn, ardından SIGKILL |

### 7.12 Sınırlar (özet)

| Sınır | Değer |
|---|---|
| Servis (port tüneli) sayısı | 8 |
| Tünel başına TCP bağlantı | 16 |
| Çerçeve yükü | 60000 bayt |
| Hello | 4096 bayt |
| Dosya boyutu | `max_file_mb`, varsayılan 64 |
| Klasör toplamı | `max_total_mb`, varsayılan 1024 |
| `file.list` kaydı | 500 |
| Eşzamanlı kontrol isteği | 4 |
| Eşzamanlı exec | 2 |
| exec çıktısı | Akış başına 1 MiB |
| Kabuk | 1 |
| Dosya aktarımı | Aynı anda 1 |

---

## 8. Servis (`neyber-link serve`)

### 8.1 Başlangıç sırası

1. `--config` dosyasını oku ve doğrula (§8.2). Hata varsa açıklamasını yaz ve çıkış kodu 2 ile kapan.
2. `Key::load(key_file)`. Unix'te dosya izni `0o077` ile maskelendiğinde sıfır değilse hata: "Anahtar dosyası başkalarınca okunabilir; chmod 600". Log'a parmak izi yazılır.
3. Dosya klasörünü oluştur (0700) ve içindeki `*.part` dosyalarını sil.
4. `punch::start(udp_port, None)` çağır, `node.accept(Some(tx))` ile kabul kanalını bağla (`sync_channel(4)`).
5. `--no-dht` verilmemişse `meet::start(.., Role::Host, ..)` başlat.
6. `openocd.autostart = true` ise OpenOCD'yi başlat. Başarısız olursa log'a yaz ve devam et.
7. Ana döngü: `rx.recv()` ile her `Bridged` için:
   - Deneme sınırı uygula (§7.3).
   - Ayrı bir iş parçacığında: kimlik doğrulama, hello, ardından oturum açık değilse oturumu başlat; açıksa `busy` gönder.
8. Oturum bitince meet döngüsünde `reset()` çağır ve log'a özet yaz.

### 8.2 Yapılandırma (`/etc/neyber-link/config.toml`)

```toml
key_file = "/etc/neyber-link/key"
udp_port = 0            # 0: rastgele. Sabit port, modemde UDP yönlendirmesi yapılacaksa kullanılır.
device_name = ""        # boş: makine adı
poll_secs = 4           # istemci kaydını DHT'de arama aralığı, 2..60

[[forward]]
name = "tcl"
port = 6666

[[forward]]
name = "gdb"
port = 3333

[files]
enabled = true
dir = "/var/lib/neyber-link/files"
max_file_mb = 64        # 1..1024
max_total_mb = 1024     # max_file_mb..65536

[openocd]
enabled = true
executable = "/usr/bin/openocd"
args = ["-f", "interface/stlink.cfg", "-f", "target/stm32f4x.cfg"]   # karta göre DEĞİŞTİR
tcl_port = 6666
gdb_port = 3333
autostart = false

[exec]
enabled = false
timeout_max_s = 600     # 1..3600

[shell]
enabled = false
program = "/bin/bash"
args = ["-l"]
```

**Doğrulama kuralları** (`config.rs`, serde `deny_unknown_fields`):

- `forward`: 1–8 kayıt. Ad `^[a-z0-9_-]{1,16}$` ve tekil, port 1–65535 ve tekil.
- `openocd.enabled = true` ise:
  - `executable` mutlak yol olmalı.
  - `args` içinde `-c` ile başlayan ya da `tcl_port`, `gdb_port`, `telnet_port`, `bindto` geçen öğe olmamalı. Bunları servis kendisi ekler (§8.4).
- `exec.enabled = true` ya da `shell.enabled = true` ise servis açılışta log'a uyarı yazar: "UYARI: anahtarı bilen herkes bu cihazda komut çalıştırabilir".
- `shell.program` mutlak yol olmalı.

### 8.3 Oturum

- `features` değerleri yapılandırmadan gelir. Kapalı bir yeteneğe gelen istek `disabled` hatası alır.
- Log'a yazılanlar:
  - açılış: `istemci adı, gerçek IP, sas kodu, anahtar parmak izi`
  - her `file.put` / `file.get` / `file.delete`: ad, boyut, sha256 ilk 12 karakter
  - her `exec`: argv'nin ilk öğesi ve öğe sayısı. Argümanlar yazılmaz, parola içerebilirler.
  - `shell.open` ve `shell.close`, `openocd.start` ve `openocd.stop`
  - kapanış: süre, neden, gönderilen ve alınan bayt
- Oturum bitince: kabuk süreci öldürülür, çalışan exec'ler öldürülür, `.part` dosyaları silinir.
  **OpenOCD durdurulmaz**; bir sonraki oturum kaldığı yerden devam eder.

### 8.4 OpenOCD yönetimi (`server/openocd.rs`)

Komut satırı:

```
<executable> -c "bindto 127.0.0.1" -c "tcl_port <tcl_port>" -c "gdb_port <gdb_port>" -c "telnet_port disabled" <args...>
```

- Servisin eklediği `-c` öğeleri **önce** gelir; port ve bağlama ayarları `init`'ten önce uygulanmalıdır.
- stdout ve stderr birleştirilip son 2000 satırı bellekte bir halka tamponda tutulur (`openocd.log` buradan okunur).
- Başlatmadan önce `127.0.0.1:<tcl_port>` adresine bağlanmayı dene. Bağlantı kurulursa port başka bir süreçte demektir: `already_running` hatası, mesaj "Port başka bir OpenOCD tarafından kullanılıyor".
- Süreç kendiliğinden biterse `openocd.exit` olayı gönderilir. **Kendiliğinden yeniden başlatılmaz**; USB hatasında sonsuz döngüye girmesin.
- Servis kapanırken OpenOCD de durdurulur: SIGTERM, ardından gerekirse SIGKILL.

### 8.5 systemd birimi (`packaging/linux/neyber-link.service`)

```ini
[Unit]
Description=neyber-link uzak kart servisi
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=neyber-link
Group=neyber-link
ExecStart=/usr/local/bin/neyber-link serve --config /etc/neyber-link/config.toml
Restart=on-failure
RestartSec=5
StateDirectory=neyber-link
StateDirectoryMode=0700
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/neyber-link
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

[Install]
WantedBy=multi-user.target
```

- `PrivateDevices` **kullanılmaz**: OpenOCD'nin `/dev/bus/usb` erişimine ihtiyacı var.
- Kabukta kendi kullanıcınla çalışıp kendi dosyalarını düzenlemek istersen bu drop-in kullanılır:
  `/etc/systemd/system/neyber-link.service.d/kabuk.conf`

  ```ini
  [Service]
  User=pi
  Group=pi
  ProtectHome=no
  ProtectSystem=full
  ```

  Ardından `chown pi:pi /etc/neyber-link/key` yapılmalı.

### 8.6 Kurulum betiği (`packaging/linux/install-link.sh`)

Root olarak çalışır ve **idempotent**tir: ikinci çalıştırmada hiçbir şeyi bozmaz, anahtarı yenilemez. Adımlar:

1. Binary verilen yoldan `/usr/local/bin/neyber-link` konumuna 0755 izinle kopyalanır.
2. `neyber-link` sistem kullanıcısı yoksa oluşturulur: `useradd --system --home-dir /var/lib/neyber-link --shell /usr/sbin/nologin neyber-link`.
3. `plugdev` grubu varsa kullanıcı bu gruba eklenir (ST-Link udev kuralları genelde bu grubu kullanır). Grup yoksa uyarı yazılır: "ST-Link erişimi için OpenOCD udev kurallarını kontrol edin".
4. `/etc/neyber-link/` 0750 izinle oluşturulur. `config.toml` yoksa örnek dosya kopyalanır.
5. `/etc/neyber-link/key` yoksa `neyber-link keygen --out /etc/neyber-link/key` çalıştırılır ve anahtar ekrana **bir kez** yazdırılır. Ardından `chown neyber-link:neyber-link`, `chmod 600`.
6. Birim dosyası `/etc/systemd/system/` altına kopyalanır, `systemctl daemon-reload` ve `systemctl enable --now neyber-link` çalıştırılır.
7. Son satır: "Yapılandırmadaki [openocd].args değerini kartınıza göre düzenleyin, ardından: systemctl restart neyber-link".

---

## 9. İstemci

### 9.1 Komutlar

Genel bayraklar:

- `--session <ad>`: varsayılan `default`, kurala `^[a-z0-9_-]{1,32}$` uyar.
- `--json`: makine çıktısı.
- `--state-dir <yol>`: varsayılan `dirs::state_dir()` ya da `dirs::data_local_dir()`, ardından `/neyber-link`.

| Komut | İş |
|---|---|
| `keygen --out <yol> [--force]` | Anahtar üretir, 0600 izinle kaydeder ve stdout'a bir kez yazar. Dosya varsa ve `--force` yoksa hata verir. |
| `import-key --out <yol> [--force]` | stdin'den anahtar okur. TTY ise `rpassword` ile gizli giriş alır. Doğrular, kaydeder ve yalnız parmak izini yazar. |
| `serve --config <yol> [--no-dht]` | §8 |
| `connect --key-file <yol> [--map ad=port]... [--timeout 60] [--peer ip:port]... [--no-dht] [--name <ad>]` | Ön planda çalışan oturum daemon'u (§9.2) |
| `status` | Durum dosyası ve yerel kontrol üzerinden: bağlı mı, servisler, sas, karşı adres |
| `disconnect` | Yerel kontrole `shutdown` gönderir; 5 sn içinde süreç bitmezse SIGTERM |
| `info` | Uzak `info` |
| `ls` | `file.list` |
| `put <yerel> [--name <ad>]` | Ad verilmezse yerel dosya adı kullanılır (kurala uymazsa hata). Çıktıda uzak `path` ve `sha256`. |
| `get <ad> [--out <yol>] [--force]` | `--out` verilmezse `./<ad>` |
| `rm <ad>` | `file.delete` |
| `openocd start\|stop\|status\|log [--lines N]` | §7.6 |
| `exec [--cwd <göreli>] [--timeout S] -- <argv...>` | Çıktı canlı olarak stdout ve stderr'e basılır, süreç uzaktaki çıkış kodunu alır. `--json` ile sonda tek bir JSON (stdout, stderr metinleri dahil). |
| `shell` | §7.10 |

`--peer` ve `--no-dht` gizli bayraklardır (`#[arg(hide = true)]`): test ve LAN kullanımı içindir.
`--peer` verilirse istemci DHT'yi beklemeden o adrese `node.dial` yapar.

### 9.2 `connect` daemon'u

1. Anahtarı yükle. Biçim veya sağlama hatasında çıkış kodu 3.
2. `--map` değerlerini ayrıştır.
3. `punch::start(0, None)` çağır, adayları bekle (en fazla 10 sn). `--peer` verildiyse hemen `dial`, ardından DHT buluşmasını başlat.
4. `--timeout` içinde `Bridged` gelmezse çıkış kodu 4.
5. Kimlik doğrulama (§7.3) ve hello (§7.4). `status` değerine göre çıkış kodu 6 ya da 7.
6. Tünelleri kur (§7.7).
7. Yerel kontrol soketini aç (§9.3) ve durum dosyasını yaz (§9.4).
8. `ready` olayını bas.
9. Oturum bitene ya da `shutdown` gelene kadar çalış. Oturum koparsa çıkış kodu 9; `shutdown` ile biterse 0. Her iki durumda da durum dosyası silinir.

**`--json` olayları** (stdout, satır başına bir JSON):

```json
{"event":"searching","fingerprint":"1a2e2e79"}
{"event":"ready","device":"lab-pi","peer":"203.0.113.7:51234","via":"quic","sas":"482 913","services":[{"name":"tcl","remote_port":6666,"local_port":6666},{"name":"gdb","remote_port":3333,"local_port":3333}],"features":{"files":true,"openocd":true,"exec":false,"shell":false},"files_dir":"/var/lib/neyber-link/files"}
{"event":"closed","code":9,"reason":"Bağlantı koptu (15 sn yanıt yok)"}
```

İnsan çıktısında (`--json` yokken) aynı bilgi Türkçe satırlarla yazılır.

**Çıkış kodları** (`connect`):

| Kod | Anlam |
|---|---|
| 0 | Normal kapanış |
| 2 | Kullanım veya yapılandırma hatası |
| 3 | Anahtar biçimi hatalı |
| 4 | Servis bulunamadı (zaman aşımı) |
| 5 | Kimlik doğrulama başarısız |
| 6 | Servis meşgul |
| 7 | Protokol sürümü uyumsuz |
| 8 | Yerel port açılamadı |
| 9 | Bağlantı koptu |

Diğer komutlar: 0 başarı, 1 uzak hata ya da doğrulama hatası, 2 kullanım hatası, 10 bağlı oturum yok.

### 9.3 Yerel kontrol soketi

- Adres `127.0.0.1:0` (rastgele port). Jeton: 32 bayt rastgele, hex.
- İstemci komutları (`put`, `exec`...) satır başına bir JSON gönderir: `{"token":"...","op":"put","path":"/mutlak/yerel","name":"fw.axf"}`
- Jeton sabit zamanlı karşılaştırılır. Yanlışsa bağlantı yanıtsız kapatılır.
- Daemon ilerleme satırları ve tek bir son satır döner:
  - ilerleme: `{"progress":<bayt>,"total":<bayt>}`
  - son: `{"ok":true,...}` ya da `{"ok":false,"code":"..","error":".."}`
- Yerel op adları uzak op adlarıyla aynıdır. Ek olarak `status` ve `shutdown` vardır.
- `put` ve `get` için yerel dosyayı daemon kendisi okur ve yazar. Yol mutlak olmalı; aynı kullanıcı olduğu için ek izin denetimi yoktur.
- `exec`: daemon EXEC_OUT verisini `{"stdout":"<base64>"}` ya da `{"stderr":"<base64>"}` satırları olarak aktarır.
- `shell`: başarılı yanıt satırından sonra bu bağlantı **ham bayt** moduna geçer; iki yönde PTY verisi akar. Boyut değişikliği ayrı bir bağlantıyla `{"op":"shell.resize","cols":..,"rows":..}` olarak gönderilir.

### 9.4 Durum dosyası

`<state-dir>/<session>.json`, 0600 izinli, geçici dosyaya yazılıp yeniden adlandırılarak oluşturulur:

```json
{"pid":12345,"control_port":40123,"token":"<64 hex>","device":"lab-pi","peer":"203.0.113.7:51234","via":"quic","sas":"482 913","fingerprint":"1a2e2e79","started_at":"2026-11-01T10:00:00Z","services":[{"name":"tcl","remote_port":6666,"local_port":6666}],"features":{"files":true,"openocd":true,"exec":false,"shell":false},"files_dir":"/var/lib/neyber-link/files"}
```

- Dosyada **anahtar yoktur**. Jeton yalnız bu oturum içindir ve oturum bitince geçersiz olur.
- Komutlar durum dosyasını bulamazsa ya da kontrol portuna bağlanamazsa çıkış kodu 10 ile döner: "Bağlı oturum yok: önce neyber-link connect".

---

## 10. EmbeddedAiTool entegrasyonu

Repo: `<EmbeddedAiTool reposu>`. O reponun
`AGENTS.md` ve `CLAUDE.md` dosyalarındaki kurallar geçerlidir: unittest + ruff, mutasyonlar
`--execute` ister, `sessions/` ve `projects/` git dışıdır, Tcl RPC telnet ile karıştırılmaz.

### 10.1 Yeni modül: `src/embedded_ai_tool/remote.py`

- `binary(explicit=None) -> list[str]`: komut öneki döner. Öncelik sırası: `--binary` argümanı, `EAT_NEYBER_LINK` ortam değişkeni, `shutil.which("neyber-link")`. Hiçbiri yoksa `ValueError("neyber-link bulunamadı")`. Testler için liste döner (`[sys.executable, "sahte.py"]` verilebilsin).
- `call(prefix, session, args, timeout=600) -> dict`: `[*prefix, "--session", session, "--json", *args]` çalıştırır, stdout'un son JSON satırını döner. Çıkış kodu 0 değilse `ValueError(<error alanı>)`.
- `connect(prefix, session, key_file, maps, timeout, log_dir) -> dict`:
  - `subprocess.Popen` ile arka planda başlatır: `stdin=DEVNULL`, stdout ve stderr `log_dir/<session>.log` dosyasına, POSIX'te `start_new_session=True`, Windows'ta `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`.
  - Log dosyasını izler; `ready` olayı gelince döner. Süreç önce biterse `closed` satırındaki `reason` ile `ValueError`. `timeout + 5` sn içinde `ready` gelmezse süreci sonlandırır ve `ValueError` verir.
- `status`, `disconnect`, `put`, `get`, `ls`, `openocd(action)`, `exec(argv, cwd, timeout)`: hepsi `call` üzerinden.

### 10.2 CLI (`cli.py`)

`remote` alt komutu, kendi alt komutlarıyla (`add_subparsers(dest="remote_command", required=True)`):

| Komut | `--execute` gerekir mi |
|---|---|
| `eat remote connect --key-file K [--session S] [--map tcl=16666 ...] [--timeout 60] [--binary B]` | Hayır |
| `eat remote status [--session S]` | Hayır |
| `eat remote disconnect [--session S]` | Hayır |
| `eat remote ls` | Hayır |
| `eat remote get NAME --out PATH [--force]` | Hayır (yalnız yerel dosya yazar) |
| `eat remote put FILE [--name N]` | **Evet** |
| `eat remote rm NAME` | **Evet** |
| `eat remote openocd status\|log` | Hayır |
| `eat remote openocd start\|stop` | **Evet** |
| `eat remote exec [--cwd C] [--timeout S] -- ARGV...` | **Evet** |

- `--execute` verilmeyen mutasyonlar mevcut desenle `{"plan": {...}, "executed": false}` döner ve hiçbir şey çalıştırmaz.
- `remote` komutları `--profile` istemez. `run()` içinde `args.profile is None` kontrolünden **önce** ele alınır.
- `main()` her komutun argümanlarını `sessions/` altına denetim kaydı olarak yazar. Kayda yalnız **anahtar dosyasının yolu** girer, anahtar metni asla girmez.
- `doctor` çıktısına iki alan eklenir: `"neyber_link"` (bulunan yol ya da `null`) ve `"neyber_link_version"` (`neyber-link --version`).
- Etkileşimli kabuk için `eat` sarmalayıcısı yoktur: `eat` JSON aracıdır. README, insanlar için `neyber-link --session S shell` komutunu anlatır.

### 10.3 Uzak kartta flash

Profil (`board.json`) şemasına iki isteğe bağlı alan eklenir:

```json
"connection": {"host": "127.0.0.1", "port": 6666, "timeout_s": 20},
"openocd": {"executable": "openocd", "config_files": [], "shared_filesystem": false, "remote_files": true},
"remote": {"session": "lab"}
```

**Doğrulama:**

- `remote_files` bool olmalı.
- `remote_files` ve `shared_filesystem` aynı anda `true` olamaz.
- `remote_files = true` ise `remote.session` zorunludur ve `^[a-z0-9_-]{1,32}$` kuralına uyar.

**`flash` komutu, `remote_files = true` iken:**

1. Plan (`--execute` olmadan):
   `{"image":..., "sha256":..., "remote_name": "eat-<sha256 ilk 16>.<elf|axf>", "effect": "upload to remote, halt, flash erase/write, verify_image, reset run"}`
   Uzantı yerel dosyanınkidir (`.axf` ya da `.elf`); başka bir uzantıysa `.elf` kullanılır.
2. `--execute` ile:
   1. `remote.put` ile dosya yüklenir.
   2. Yanıttaki `sha256` değeri `image.sha` ile **eşleşmiyorsa** RPC'ye hiç dokunmadan hata verilir.
   3. Yanıttaki `path` değeri mutlak değilse hata verilir.
   4. Mevcut Tcl betiği uzak yolla çalıştırılır: `device.guard() + "halt; flash write_image erase {p}; verify_image {p}; reset run"`, burada `p = tcl_string(path)`.
3. `remote_files = false` iken mevcut davranış hiç değişmez.

**GDB ile hata ayıklama** (README'ye yazılır): ELF dosyası yerelde kalır, GDB sunucusu uzaktadır.

```
arm-none-eabi-gdb build/firmware.axf -ex "target extended-remote 127.0.0.1:3333"
```

### 10.4 Testler (`tests/test_remote.py`)

Sahte bir neyber-link kullanılır: test geçici klasöre bir Python betiği yazar ve önek `[sys.executable, betik]` olur.
Betik `connect` için `ready` satırı basıp SIGTERM gelene kadar bekler; diğer komutlar için sabit JSON döner.

Testler:

1. `connect` → `status` → `disconnect` akışı çalışır.
2. Süreç `ready` gelmeden biterse `reason` metni hataya yansır.
3. `put` / `rm` / `exec` / `openocd start` komutları `--execute` olmadan alt süreç **başlatmaz**.
4. Uzak flash: sahte `put` yanlış SHA-256 döndürünce `rpc.call` hiç çağrılmaz (Mock ile doğrulanır).
5. Uzak flash: doğru SHA-256 ile Tcl betiği `tcl_string(uzak_yol)` içerir.
6. `remote_files` ve `shared_filesystem` ikisi birden `true` ise profil reddedilir.
7. Anahtar metni (`NL1-` ile başlayan) `sessions/` altındaki hiçbir dosyada geçmez.

Kontrol: `python -m unittest discover -s tests -v` ve `python -m ruff check src tests`.
Sonuçlar `docs/DOGRULAMA.md` dosyasına eklenir. Sentetik testin kart testi olmadığı açıkça yazılır.

---

## 11. Güvenlik modeli

| Tehdit | Önlem |
|---|---|
| Anahtar ele geçirilirse | Anahtar kartın ve açık yeteneklere göre cihazın tam yetkisidir: flash, bellek yazma, dosya yazma, `exec`/`shell` açıksa komut çalıştırma. Bu yüzden `exec` ve `shell` varsayılan kapalıdır. Servis ayrı ve yetkisiz bir kullanıcıyla çalışır. Sızıntı şüphesinde `keygen --force` ve `systemctl restart neyber-link`. |
| DHT üzerinden adres keşfi | Kayıt, anahtardan türeyen anahtarla şifreli ve imzalıdır. Anahtarı bilmeyen kaydın yerini bile bulamaz. 128 bit rastgele anahtar tahmin edilemez. DHT düğümleri yalnız kaydı yazanın IP adresini görür. |
| Ortadaki adam | SPAKE2 + Noise: anahtarı bilmeyen araya giremez. `sas` kodu iki tarafta da gösterilir ve istenirse karşılaştırılır. |
| Kaba kuvvet ya da gürültü | Gerçek IP başına ve genel deneme sınırları (§7.3). İmzasız bağlantılar sessizce kapatılır. |
| Uzak ağa sızma (pivot) | Tüneller yalnız yapılandırmadaki `127.0.0.1` portlarına gider; istemci hedef seçemez (`tunnel.rs` hedefi `allowed_port` ile kısıtlar). |
| Klasör dışına dosya yazma | Ad kuralında yol ayırıcı yoktur; `exec` için `cwd` kanonik yol kontrolünden geçer. |
| Uzak cihazdaki diğer yerel kullanıcılar | OpenOCD `127.0.0.1` adresine bağlıdır. Uzak cihazdaki başka bir yerel kullanıcı OpenOCD portuna zaten erişebilir; bu, neyber-link'ten bağımsız, mevcut bir durumdur ve belgelenir. |
| İstemci makinesindeki diğer kullanıcılar | Yerel tünel portları `127.0.0.1` adresine bağlıdır ve aynı makinedeki başka kullanıcılar bağlanabilir. Kontrol soketi jetonla korunur, jeton 0600 izinli dosyadadır. Paylaşılan makinelerde bu risk README'de yazılır. |
| Log sızıntısı | Anahtar, PAKE sırrı, jeton ve exec argümanları log'a yazılmaz. |

---

## 12. Fazlar

Her faz sonunda, Ulac'ta `cargo build --workspace` ve `cargo test --workspace` geçmelidir.

### Faz 0 — Hazırlık

1. `git -C <ulac reposu> status` çalıştır. Commit edilmemiş `ulac → neyber` yeniden adlandırması varsa **kullanıcıya sor**: önce commit'lensin mi? Onaysız commit atma.
2. Yeni dal aç: `git -C <ulac reposu> switch -c neyber-link`.
3. Başlangıç durumunu kaydet: `cargo test` çıktısını rapora koy.

**Kabul:** temiz bir dal ve mevcut testlerin geçtiğini gösteren kayıt.

### Faz 1 — `neyber-core` crate'ini ayırmak (davranış değişmez)

1. `crates/neyber-core/Cargo.toml` dosyasını oluştur:

   ```toml
   [package]
   name = "neyber-core"
   version = "0.1.0"
   edition = "2024"
   publish = false

   [dependencies]
   blake2 = "0.11.0"
   chacha20poly1305 = "0.11.0"
   getrandom = "0.4.3"
   mainline = "8"
   quinn = { version = "0.11", default-features = false, features = ["runtime-tokio", "rustls", "ring"] }
   rcgen = { version = "0.14", default-features = false, features = ["ring"] }
   snow = { version = "0.10.0", default-features = false, features = ["default-resolver", "use-chacha20poly1305", "use-blake2", "use-curve25519", "use-getrandom"] }
   socket2 = "0.6"
   spake2 = "0.4.0"
   tokio = { version = "1", features = ["rt", "net", "io-util", "time", "sync", "macros"] }
   ```

   Sürümleri yazmadan önce kök `Cargo.toml` ile karşılaştır; aynı olmalılar.
2. Dosyaları taşı (tek komut): `git -C <ulac reposu> mv src/net.rs src/punch.rs src/portmap.rs src/rendezvous.rs src/stun.rs src/tunnel.rs crates/neyber-core/src/`
3. `crates/neyber-core/src/lib.rs` dosyasını yaz:

   ```rust
   //! Neyber ağ çekirdeği: NAT delme, DHT buluşması, parolalı şifreli kanal ve TCP tüneli.
   pub mod addr;
   pub mod log;
   pub mod net;
   pub mod portmap;
   pub mod punch;
   pub mod rendezvous;
   pub mod stun;
   pub mod tunnel;
   ```

4. `addr.rs`: `direct.rs` içindeki `candidates_text` ve `show_addr` birebir taşınır. `parse_candidates` gövdesi `parse_list` adıyla taşınır, ancak `REPLY_PREFIX` ayıklama satırı **çıkarılır** (o kısım Ulac'a özel). Hata mesajları aynı kalır.
5. `log.rs`:

   ```rust
   use std::sync::OnceLock;

   static SINK: OnceLock<fn(&str)> = OnceLock::new();

   /// Uygulamanın günlüğünü bağlar; bağlanmazsa satırlar atılır (testler dahil).
   pub fn set_logger(sink: fn(&str)) {
       let _ = SINK.set(sink);
   }

   pub fn log(line: impl AsRef<str>) {
       if let Some(sink) = SINK.get() {
           sink(line.as_ref());
       }
   }
   ```

6. `punch.rs` değişiklikleri: `crate::direct::log(` → `crate::log::log(`, `crate::direct::candidates_text` → `crate::addr::candidates_text`, `crate::direct::show_addr` → `crate::addr::show_addr`.
7. `rendezvous.rs` değişiklikleri: `crate::direct::candidates_text` → `crate::addr::candidates_text`, `crate::direct::parse_candidates` → `crate::addr::parse_list`.
8. `rendezvous.rs` içine bağlam parametreli türetme ekle:
   - `pub fn keys_in(context: &[u8], username: &str, password: &str) -> Keys` mevcut gövdeyi alır; `b"neyber/rendezvous/v1\0"` sabitinin yerine `context` kullanılır.
   - `pub fn keys(u, p) -> Keys { keys_in(b"neyber/rendezvous/v1\0", u, p) }` olur.
   - Test ekle: aynı girdilerle `keys` ve `keys_in(eski bağlam)` çağrılarının `signer.verifying_key().to_bytes()` değerleri eşit; farklı bağlamla farklı.
9. Kök `Cargo.toml` dosyasına ekle:

   ```toml
   [workspace]
   members = ["crates/neyber-core"]
   ```

   `"crates/neyber-link"` bu listeye Faz 2'de eklenir. Var olmayan bir üye yazılırsa cargo hata verir.
   `[dependencies]` altına `neyber-core = { path = "crates/neyber-core" }` ekle.
   Kökteki diğer bağımlılıklara **dokunma**. `[patch.crates-io]` ve `[profile.*]` kökte kalır.
10. `src/main.rs`:
    - `mod net; mod portmap; mod punch; mod rendezvous; mod stun; mod tunnel;` satırlarını sil, yerine `use neyber_core::{net, portmap, punch, rendezvous, stun, tunnel};` ekle. Böylece `crate::punch::...` yolları çalışmaya devam eder.
    - `main()` başına ekle: `neyber_core::log::set_logger(|line| direct::log(line));`
11. `src/direct.rs`:
    - `candidates_text` ve `show_addr` tanımlarını sil, yerine `pub use neyber_core::addr::{candidates_text, show_addr};` ekle.
    - `parse_candidates` gövdesi şu olur: `REPLY_PREFIX` ayıklanır, ardından `neyber_core::addr::parse_list(text)` döner.
12. Derle. Görünürlük hatası çıkan öğeleri (ör. `pub(crate)`) yalnız gerektiği kadar `pub` yap.
13. `.github/workflows/ci.yml` dosyasında `cargo test` ya da `cargo clippy` adımları varsa `--workspace` ekle.

**Kabul:**

- `cargo build` ve `cargo test --workspace` geçer.
- Taşınan modüllerin testleri `neyber-core` altında koşar.
- `cargo build --release` sonrası `neyber` uygulaması açılır ve bir LAN oturumu eskisi gibi kurulur. Bu adım elle yapılır; yapılamadıysa raporda belirtilir.
- `git diff` içinde `net.rs` ve `tunnel.rs` yalnız taşınmış görünür (`git log --follow` ile).

### Faz 2 — `neyber-link` iskeleti: anahtar ve yapılandırma

1. Kök `Cargo.toml` dosyasında `members` listesine `"crates/neyber-link"` ekle. `crates/neyber-link/Cargo.toml` (`[package]` alanları neyber-core ile aynı düzende, `name = "neyber-link"`) içine `neyber-core = { path = "../neyber-core" }` yaz. Ardından `cargo add -p neyber-link clap --features derive`, `serde --features derive`, `serde_json`, `toml`, `sha2`, `blake2`, `getrandom`, `dirs`, `rpassword`.
2. `key.rs` (§7.1):
   - `Key::generate()`, `Key::parse(&str) -> Result<Key, String>`, `text()`, `fingerprint()`, `pake_secret()`, `rendezvous() -> rendezvous::Keys`, `load(&Path)`, `save_new(&Path, force)`.
   - `Debug` elle yazılır ve yalnız parmak izini gösterir.
   - Unix'te dosya `OpenOptions::mode(0o600)` ile oluşturulur.
3. `config.rs` (§8.2): serde yapıları, `validate()`. Hata mesajları alan adını içerir.
4. `main.rs`: §9.1'deki tüm komutlar clap ile tanımlanır. Bu fazda yalnız `keygen` ve `import-key` çalışır; diğerleri "henüz yok" mesajıyla çıkış kodu 2 döner. `--version` çalışır.
5. Testler:
   - test vektörü birebir
   - küçük harf, boşluk ve tire toleransı
   - `O/I/L` dönüşümü
   - tek karakter değişince sağlama hatası
   - yanlış önek, yanlış uzunluk
   - `Debug` çıktısında sırrın hex hâli geçmiyor
   - 0644 izinli dosya reddediliyor (Unix)
   - örnek `neyber-link.toml` geçerli
   - her doğrulama kuralı için bir ret testi

**Kabul:** `cargo test -p neyber-link` geçer ve `neyber-link keygen --out <scratch>/k` 0600 izinli dosya üretir.

### Faz 3 — Oturum çekirdeği ve port tünelleri

1. `wire.rs`: kanal sabitleri, `write_frame`, `read_frame`, `write_hello`, `read_hello` ve sınır kontrolleri.
2. `auth.rs`: §7.3 (imza, `Deadline`, el sıkışma, deneme sınırı yapısı `Limiter`).
3. `meet.rs`: §7.2.
4. `server/session.rs` ve `client/session.rs`:
   - tek yazıcı ve klon okuyucu
   - PING iş parçacığı
   - okuma döngüsü kanalları dağıtır
   - TUNNEL kanalları (§7.7)
   - CONTROL kanalında yalnız `info`; diğer op'lar `bad_request`
5. `serve` ve `connect` komutlarını bağla. `connect` bu fazda yerel kontrol soketi açmaz; yalnız `ready` olayını basar ve çalışmayı sürdürür.
6. `tests/loopback.rs`:
   - aynı süreçte servis ve istemci
   - `punch::start(0, Some(vec![loopback]))`, `--peer` eşdeğeri doğrudan `dial`, DHT kapalı
   - sahte bir TCP yankı sunucusu (§Ek A'nın Rust karşılığı) üzerinden iki servis
   - doğrulanacaklar: veri iki yönde akar; yarı kapatma korunur; yanlış anahtar 5 koduyla düşer; ikinci istemci `busy` alır; servis ölünce istemci 15 sn içinde 9 koduyla çıkar; servis yeniden başlayınca yeni oturum kurulur

**Kabul:**

- Testler geçer.
- Elle yapılan şu deneme tutar: Ek A'daki sahte Tcl sunucusu `16666` portunda çalışırken `serve --no-dht` ve `connect --no-dht --peer 127.0.0.1:<udp_port> --map tcl=26666` sonrası, `26666` portuna `hello\x1a` gönderilince `OK\n5\x1a` döner.

### Faz 4 — Yerel kontrol soketi ve oturum komutları

1. `client/control.rs` (§9.3) ve `client/state.rs` (§9.4).
2. `status`, `disconnect`, `info` komutları.
3. Testler: yanlış jeton yanıtsız kapanır; `disconnect` sonrası durum dosyası silinir; oturum yokken komut 10 koduyla döner; durum dosyası 0600 izinli ve içinde `NL1-` geçmiyor.

**Kabul:** testler geçer. Elle: `connect` açıkken başka bir terminalde `status` bağlı görünür, `disconnect` süreci kapatır.

### Faz 5 — Dosya aktarımı

1. `server/files.rs` ve istemci tarafı: `put`, `get`, `ls`, `rm` (§7.8). İlerleme satırları gösterilir.
2. Testler:
   - 0 bayt, 1 bayt, 8192, 8193 ve 5 MiB rastgele dosya iki yönde aynı SHA-256 ile gelir
   - geçersiz adlar (`../x`, `.gizli`, `a/b`, `x.part`, 129 karakter) reddedilir
   - `max_file_mb` aşımı `too_large`
   - aktarım ortasında oturum kesilince `.part` kalmaz
   - aktarım sırasında tünel üzerinden veri akmaya devam eder

**Kabul:** testler geçer.

### Faz 6 — OpenOCD yönetimi

1. `server/openocd.rs` (§8.4): `start`, `stop`, `status`, `log`, `openocd.exit` olayı, servis kapanırken temizlik.
2. Testler gerçek OpenOCD yerine sahte bir çalıştırılabilir dosya kullanır: test geçici klasöre, verilen `tcl_port` portunu dinleyen ve satır basan küçük bir betik yazar.
   Doğrulanacaklar: komut satırı sırası (servisin `-c` öğeleri önce); `already_running`; 10 sn içinde port açılmazsa `start_failed` ve son satırlar; `stop` sonrası süreç yok.
3. Gerçek OpenOCD ile deneme bu fazda yapılmaz. Faz 10'da **kullanıcının kartındaki gerçek yapılandırmayla** denenir; OpenOCD argümanlarını ajan uydurmaz.

**Kabul:** testler geçer; yapılandırma doğrulaması `args` içindeki `-c` öğesini reddeder.

### Faz 7 — exec ve etkileşimli kabuk

1. `cargo add -p neyber-link portable-pty crossterm signal-hook`
2. `server/exec.rs` (§7.9) ve istemci `exec` komutu: canlı çıktı, uzaktaki çıkış kodunu yansıtma.
3. `server/shell.rs` ve istemci `shell` komutu (§7.10).
4. Testler:
   - exec: `["sh","-c","echo a; echo b >&2; exit 3"]` → stdout `a\n`, stderr `b\n`, kod 3
   - `timeout_s=1` ile `sleep 5` → `timed_out` ve süreç grubu öldürülmüş
   - `cwd="../"` → `bad_request`
   - kapalıyken `disabled`
   - 2 MiB çıktı → `truncated`
   - kabuk (Unix): `shell.open`, `echo merhaba\nexit\n` yaz, çıktıda `merhaba` görünür ve `shell.exit` gelir
   - kabuk kapalıyken `disabled`

**Kabul:** testler geçer; elle `neyber-link shell` içinde `vim` açılıp kapanır ve terminal düzgün geri gelir.

### Faz 8 — Paketleme

1. `packaging/linux/neyber-link.service` (§8.5), `packaging/linux/neyber-link.toml` (§8.2'deki örnek) ve `packaging/linux/install-link.sh` (§8.6).
2. `install-link.sh` için `shellcheck` temiz olmalı (kuruluysa).
3. `.github/workflows/release.yml`: mevcut Linux hedeflerinde `cargo build --release --locked -p neyber-link` adımı ve `neyber-link-v$VERSION-<hedef>` artefaktı eklenir. aarch64 runner yoksa README'ye "Raspberry Pi'de yerel derleme: `cargo build --release -p neyber-link`" yazılır.
4. Ulac `README.md`: kısa bir "neyber-link (uzak kart servisi)" bölümü ve bu belgeye bağlantı.

**Kabul:** bir Linux VM ya da Pi'de `install-link.sh` iki kez çalışır; ikinci çalıştırma anahtarı değiştirmez; `systemctl status neyber-link` aktif görünür; `journalctl -u neyber-link` içinde anahtar değil, parmak izi vardır.

### Faz 9 — EmbeddedAiTool entegrasyonu

§10'un tamamı: `remote.py`, CLI, profil doğrulaması, uzak flash, testler, README ve `docs/DOGRULAMA.md`.

**Kabul:** `python -m unittest discover -s tests -v` ve `python -m ruff check src tests` geçer; mevcut 13+ test bozulmaz.

### Faz 10 — Saha testi

Kullanıcıyla birlikte yapılır, gerçek donanım ve iki farklı ağ gerekir:

1. Uzak cihaz ev veya ofis ağında; istemci telefon hotspot'unda (farklı NAT).
2. `eat remote connect`, ardından `eat identity` kimlik eşleşmesini doğrular.
3. `eat read` / `eat watch` 60 sn çalışır; örnek sayısı ve hatalar kaydedilir.
4. `eat flash --execute` (uzak) çalışır ve `verify` geçer.
5. GDB `target extended-remote` ile breakpoint, step ve continue denenir.
6. `neyber-link get` ile uzakta derlenmiş bir `.axf` alınır, `eat symbols` ile okunur.
7. `exec` ile uzakta `make` çalıştırılır (açıksa); `shell` içinde dosya düzenlenir.
8. Uzak cihaz yeniden başlatılır; servis kendiliğinden açılır ve yeniden bağlanılır.
9. Ölçümler: ilk bağlantı süresi, `read` gecikmesi, flash süresi (yerelle karşılaştırmalı).
10. Sonuçlar `docs/UZAK_KART_SERVISI_SAHA_TESTI_<tarih>.md` dosyasına yazılır. Yapılamayan adım "yapılmadı" olarak işaretlenir.

---

## 13. Test matrisi

| Katman | Nerede | İnternet | Donanım |
|---|---|---|---|
| Anahtar, yapılandırma, çerçeve, hello | `neyber-link` birim | Yok | Yok |
| Oturum, tüneller, dosyalar, exec, kabuk | `tests/loopback.rs` | Yok (loopback, DHT kapalı) | Yok |
| OpenOCD yönetimi | Sahte çalıştırılabilir dosya | Yok | Yok |
| EmbeddedAiTool remote | Sahte neyber-link betiği | Yok | Yok |
| DHT ile buluşma | Elle, `NEYBER_LINK_LIVE=1` | Var | Yok |
| Farklı NAT, gerçek kart | Faz 10 | Var | Var |

---

## 14. Bilinen sınırlar ve riskler

- **Relay yok.** İki taraf da simetrik NAT veya CGNAT arkasındaysa delik açılamayabilir. Port tahmini (`punch::window`) çoğu mobil hattı kurtarır ama garanti değildir. O zaman çözüm, uzak tarafta modemde `udp_port` için UDP yönlendirmesi ya da IPv6'dır.
- **"Sunucusuz" ama herkese açık altyapıya dayanır:** BitTorrent DHT önyükleme düğümleri ve STUN (`stun.l.google.com`, `stun.cloudflare.com` ...). Bunlara erişim yoksa yalnız `--peer` ile LAN bağlantısı kalır.
- **Bağlanma süresi:** DHT'de buluşma birkaç saniye ile birkaç on saniye sürebilir. `poll_secs` düşürülürse hızlanır ama DHT trafiği artar.
- **Tek oturum:** istemci ağı koparsa servis, eski oturumun ölü olduğunu en geç 15 sn sonra anlar; bu süre içinde yeni bağlantı `busy` alır. İstemci yeniden dener.
- **GDB gecikmesi:** adım adım ilerleme rahat çalışır; GDB ile büyük `load` yavaştır. Uzak flash için §10.3 yolu (dosyayı yükle, OpenOCD yerelde yazsın) daha hızlıdır.
- **`exec` ve `shell` açıksa anahtar, uzak cihazdaki servis kullanıcısının yetkisini verir.** §11'deki uyarı README'de de görünür olmalı.

## 15. Açık sorular (kullanıcıya)

1. Uzak cihaz kesin olarak ne (Raspberry Pi modeli ya da dağıtım)? Faz 8 ve Faz 10 buna göre şekillenir.
2. Kabukta servis kullanıcısı mı, kendi kullanıcın mı (§8.5 drop-in)?
3. İleride kişi başına ayrı anahtar ve tek tek iptal gerekecek mi? Gerekirse `keys/` klasörü ve hello'da anahtar kimliği eklenir; v1 protokolüne dokunmadan `proto: 2` ile yapılır.
4. Windows'ta servis gerekiyor mu? Gerekirse ayrı bir faz.

---

## Ek A — Sahte Tcl RPC sunucusu (elle test için)

`fake_tcl.py` olarak scratch klasörüne kaydedilir ve `python3 -I fake_tcl.py 16666` ile çalıştırılır.
Her `0x1a` ile biten komuta `OK\n<komut uzunluğu>\x1a` yanıtı verir.

```python
"""Sahte OpenOCD Tcl RPC: her 0x1a ile biten komuta 'OK\\n<uzunluk>\\x1a' döner."""
import socket
import sys
import threading


def serve(conn):
    with conn:
        buffer = b""
        while True:
            data = conn.recv(65536)
            if not data:
                return
            buffer += data
            while b"\x1a" in buffer:
                frame, buffer = buffer.split(b"\x1a", 1)
                conn.sendall(b"OK\n" + str(len(frame)).encode() + b"\x1a")


server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", int(sys.argv[1])))
server.listen()
while True:
    connection, _ = server.accept()
    threading.Thread(target=serve, args=(connection,), daemon=True).start()
```

## Ek B — Kullanım senaryosu (README'ye girecek)

**Uzak cihazda, bir kez:**

```
sudo ./install-link.sh ./neyber-link
#   Bağlantı anahtarı: NL1-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX   (bir kez gösterilir)
sudo nano /etc/neyber-link/config.toml        # [openocd].args değerini kartına göre yaz
sudo systemctl restart neyber-link
```

**Kendi bilgisayarında:**

```
neyber-link import-key --out ~/.config/neyber-link/lab.key      # anahtarı yapıştır
neyber-link --session lab connect --key-file ~/.config/neyber-link/lab.key
# başka terminalde:
neyber-link --session lab openocd start
eat --profile projects/kart/board.json identity
eat --profile projects/kart/board.json flash --execute          # remote_files: true
arm-none-eabi-gdb build/firmware.axf -ex "target extended-remote 127.0.0.1:3333"
neyber-link --session lab get firmware.axf --out build/uzak.axf
neyber-link --session lab shell                                 # [shell].enabled = true ise
neyber-link --session lab disconnect
```
