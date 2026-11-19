//! Küçük ortak yardımcılar.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `name` yalnız log içindir: 64 karakter, `[A-Za-z0-9._-]` dışı `-` olur.
pub fn clean_name(name: &str) -> String {
    name.chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `^[a-z0-9_-]{1,32}$`
pub fn valid_session(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$` ve `.part` ile bitmez
pub fn valid_file_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
        && !name.ends_with(".part")
}

pub fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&buf[..n]);
    }
}

pub fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// RFC 3339 (UTC, saniye çözünürlüğü)
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Howard Hinnant'ın civil_from_days algoritması
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

pub fn now_rfc3339() -> String {
    rfc3339(unix_secs(SystemTime::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_rules() {
        for ok in ["fw.axf", "a", "A-b_c.1"] {
            assert!(valid_file_name(ok), "{ok}");
        }
        let long = "a".repeat(129);
        for bad in ["../x", ".gizli", "a/b", "x.part", "", long.as_str(), "a b"] {
            assert!(!valid_file_name(bad), "{bad}");
        }
        assert!(valid_file_name(&"a".repeat(128)));
    }

    #[test]
    fn session_rules() {
        assert!(valid_session("lab_1-x"));
        for bad in ["", "Lab", "a b", &"a".repeat(33)] {
            assert!(!valid_session(bad));
        }
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_782_950_400), "2026-07-02T00:00:00Z");
    }
}
