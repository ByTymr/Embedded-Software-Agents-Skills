//! Bağlantı anahtarı: `OL1-` + 32 Crockford base32 karakteri (16 bayt sır + 4 bayt sağlama).

use crate::util::hex;
use blake2::{Blake2s256, Digest};
use openocd_link_core::rendezvous;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const PREFIX: &str = "OL1";

#[derive(Clone)]
pub struct Key {
    secret: [u8; 16],
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({})", self.fingerprint())
    }
}

fn digest(context: &[u8], data: &[u8]) -> [u8; 32] {
    Blake2s256::new()
        .chain_update(context)
        .chain_update(data)
        .finalize()
        .into()
}

fn check(secret: &[u8; 16]) -> [u8; 4] {
    let d = digest(b"openocd-link/key-check/v1", secret);
    [d[0], d[1], d[2], d[3]]
}

impl Key {
    pub fn from_secret(secret: [u8; 16]) -> Self {
        Self { secret }
    }

    pub fn generate() -> Self {
        let mut secret = [0u8; 16];
        getrandom::fill(&mut secret).expect("sistem rastgelelik kaynağı yok");
        Self { secret }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        // Bağlantı dosyasındaki `#` açıklama satırları yok sayılır
        let text: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let clean: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect::<String>()
            .to_ascii_uppercase();
        let Some(body) = clean.strip_prefix(PREFIX) else {
            return Err("Anahtar OL1- ile başlamalı".into());
        };
        if body.chars().count() != 32 {
            return Err("Anahtar uzunluğu yanlış".into());
        }
        let mut raw = [0u8; 20];
        let (mut acc, mut bits, mut at) = (0u32, 0u32, 0usize);
        for c in body.chars() {
            let c = match c {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            let value = ALPHABET
                .iter()
                .position(|&a| a as char == c)
                .ok_or_else(|| format!("Anahtarda geçersiz karakter: {c}"))?;
            acc = (acc << 5) | value as u32;
            bits += 5;
            while bits >= 8 {
                raw[at] = (acc >> (bits - 8)) as u8;
                at += 1;
                bits -= 8;
            }
            acc &= (1 << bits) - 1;
        }
        let mut secret = [0u8; 16];
        secret.copy_from_slice(&raw[..16]);
        if raw[16..] != check(&secret) {
            return Err("Anahtar yanlış yazılmış (sağlama tutmadı)".into());
        }
        Ok(Self { secret })
    }

    pub fn text(&self) -> String {
        let mut raw = self.secret.to_vec();
        raw.extend(check(&self.secret));
        let mut out = String::new();
        let (mut acc, mut bits) = (0u32, 0u32);
        for b in raw {
            acc = (acc << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                out.push(ALPHABET[((acc >> (bits - 5)) & 31) as usize] as char);
                bits -= 5;
            }
            acc &= (1 << bits) - 1;
        }
        let groups: Vec<&str> = (0..8).map(|i| &out[i * 4..i * 4 + 4]).collect();
        format!("{PREFIX}-{}", groups.join("-"))
    }

    /// Log ve çıktılarda anahtarın yerine geçer
    pub fn fingerprint(&self) -> String {
        hex(&digest(b"openocd-link/fingerprint/v1\0", &self.secret)[..4])
    }

    /// `net::handshake` sırrı
    pub fn pake_secret(&self) -> String {
        hex(&digest(b"openocd-link/pake/v1\0", &self.secret))
    }

    pub fn rendezvous(&self) -> rendezvous::Keys {
        rendezvous::keys_in(
            b"openocd-link/rendezvous/v1\0",
            "link",
            &hex(&self.secret),
        )
    }

    /// İstemci tarafı: başka makineden kopyalanmış dosyanın izinleri kısıtlanmaz
    pub fn load_lenient(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Anahtar dosyası okunamadı ({}): {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Servis tarafı: dosya başkalarınca okunabiliyorsa reddeder
    pub fn load(path: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| format!("Anahtar dosyası okunamadı ({}): {e}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err("Anahtar dosyası başkalarınca okunabilir; chmod 600".into());
            }
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Anahtar dosyası okunamadı ({}): {e}", path.display()))?;
        Self::parse(&text)
    }

    /// 0600 izinle yazar; dosya varsa ve `force` yoksa hata verir
    pub fn save_new(&self, path: &Path, force: bool) -> Result<(), String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut options = OpenOptions::new();
        options.write(true);
        if force {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!("{} zaten var (--force ile üzerine yazılır)", path.display())
            } else {
                format!("{}: {e}", path.display())
            }
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        writeln!(file, "{}", self.text()).map_err(|e| e.to_string())?;
        // SD kartta ani kapanmada boş dosya kalmasın
        file.sync_all().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector() -> Key {
        Key::from_secret(std::array::from_fn(|i| i as u8))
    }

    #[test]
    fn test_vector() {
        let key = vector();
        assert_eq!(hex(&check(&key.secret)), "173469f6");
        assert_eq!(key.text(), "OL1-000G-40R4-0M30-E209-185G-R38E-1WBK-8TFP");
        assert_eq!(key.pake_secret(), "be8edba94723b9b4160b58bb5f6f729c1e3131d29a3f7c3276bfee0ea60c988f");
        assert_eq!(key.fingerprint(), "dbc83e3b");
        assert_eq!(Key::parse(&key.text()).unwrap().secret, key.secret);
    }

    #[test]
    fn tolerant_reading() {
        let key = vector();
        let t = key.text();
        for variant in [t.to_lowercase(), t.replace('-', " "), t.replace('-', "")] {
            assert_eq!(Key::parse(&variant).unwrap().secret, key.secret, "{variant}");
        }
        // O/I/L karışıklığı: 0 ve 1 yerine yazılabilir
        let confusable = t.replace('0', "O").replace('1', "l");
        let confusable = format!("OL1{}", &confusable[3..]);
        assert_eq!(Key::parse(&confusable).unwrap().secret, key.secret);
    }

    #[test]
    fn single_character_change_fails_checksum() {
        let t = vector().text();
        let mut chars: Vec<char> = t.chars().collect();
        let at = chars.len() - 1;
        chars[at] = if chars[at] == '0' { '1' } else { '0' };
        let bad: String = chars.into_iter().collect();
        assert!(Key::parse(&bad).unwrap_err().contains("sağlama"));
    }

    #[test]
    fn wrong_prefix_and_length() {
        assert!(Key::parse("NL1-0000-0000").unwrap_err().contains("OL1-"));
        assert!(Key::parse("OL1-0000").unwrap_err().contains("uzunluğu"));
        let bad = format!("OL1-{}", "U".repeat(32));
        assert!(Key::parse(&bad).unwrap_err().contains("geçersiz karakter"));
    }

    #[test]
    fn comment_lines_in_connection_files_are_ignored() {
        let key = vector();
        let text = format!("# openocd-link bağlantı dosyası\n# cihaz: pi\n{}\n", key.text());
        assert_eq!(Key::parse(&text).unwrap().secret, key.secret);
    }

    #[test]
    fn debug_hides_secret() {
        let key = vector();
        let shown = format!("{key:?}");
        assert!(!shown.contains(&hex(&key.secret)));
        assert!(shown.contains(&key.fingerprint()));
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_enforced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ol-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k");
        let key = Key::generate();
        key.save_new(&path, false).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(key.save_new(&path, false).is_err());
        assert_eq!(Key::load(&path).unwrap().secret, key.secret);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Key::load(&path).unwrap_err().contains("chmod 600"));
        std::fs::remove_dir_all(dir).ok();
    }
}
