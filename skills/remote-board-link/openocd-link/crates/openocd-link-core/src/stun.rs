//! STUN (RFC 8489) bağlama isteği: modemin bu UDP soketini internette hangi adres:port ile
//! gösterdiğini öğrenir. Yalnız 20 baytlık sorgu gider; içerik, kimlik veya parola gitmez.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Herkese açık, ücretsiz STUN noktaları; biri yanıt vermezse sıradaki denenir
const SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun1.l.google.com:19302",
    "stun.nextcloud.com:3478",
];
const MAGIC: u32 = 0x2112_A442;

/// Soketin dış IPv4 adresi. Soket çift yığınlı olabilir: IPv4 hedefi eşlenmiş adresle gönderilir.
pub fn mapped_v4(socket: &UdpSocket) -> Option<SocketAddr> {
    query(socket, SERVERS)
}

/// Bu ağın NAT'ı simetrik mi: aynı soket iki farklı sunucuya farklı dış portla görünüyorsa
/// öyledir (çoğu mobil hat). Dönen adres son görülen eşlemedir; simetrik NAT'larda sıradaki
/// eşleme genelde onun hemen ardından gelir, bekleyen taraf o aralığa delik açar.
/// Karşılaştırma için iki ayrı sunucudan yanıt gerekir; yalnız biri yanıt verirse sonuç
/// belirsizdir (`None`), "koni" sayılmaz.
pub fn probe_nat() -> Option<(SocketAddr, Option<bool>)> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    let mut seen: Vec<SocketAddr> = Vec::with_capacity(2);
    for server in SERVERS {
        if let Some(addr) = query(&socket, std::slice::from_ref(server)) {
            seen.push(addr);
            if seen.len() == 2 {
                break;
            }
        }
    }
    let last = *seen.last()?;
    Some((last, (seen.len() == 2).then(|| seen[0].port() != seen[1].port())))
}

fn query(socket: &UdpSocket, servers: &[&str]) -> Option<SocketAddr> {
    let dual = socket.local_addr().ok()?.is_ipv6();
    let mut id = [0u8; 12];
    getrandom::fill(&mut id).ok()?;
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&1u16.to_be_bytes()); // Binding Request
    request.extend_from_slice(&0u16.to_be_bytes());
    request.extend_from_slice(&MAGIC.to_be_bytes());
    request.extend_from_slice(&id);
    let targets: Vec<SocketAddr> = servers
        .iter()
        .filter_map(|s| s.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4))
        .map(|a| match (dual, a.ip()) {
            (true, IpAddr::V4(v4)) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), a.port()),
            _ => a,
        })
        .collect();
    let previous = socket.read_timeout().ok().flatten();
    let _ = socket.set_read_timeout(Some(Duration::from_millis(300)));
    let mut found = None;
    'tries: for _ in 0..3 {
        for target in &targets {
            let _ = socket.send_to(&request, target);
        }
        let until = Instant::now() + Duration::from_millis(900);
        let mut buf = [0u8; 512];
        while Instant::now() < until {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            if targets.contains(&from)
                && let Some(addr) = parse(&buf[..n], &id)
            {
                found = Some(addr);
                break 'tries;
            }
        }
    }
    let _ = socket.set_read_timeout(previous);
    found
}

/// Eşlemeyi canlı tutmak için gönderilen tek paket; yanıtı beklenmez
pub fn binding_request() -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[..2].copy_from_slice(&1u16.to_be_bytes());
    packet[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    let _ = getrandom::fill(&mut packet[8..]);
    packet
}

/// Canlı tutma paketlerinin gideceği STUN noktası
pub fn keepalive_target() -> Option<SocketAddr> {
    SERVERS
        .iter()
        .find_map(|s| s.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4))
}

/// Bağlama yanıtındaki (XOR-)MAPPED-ADDRESS
fn parse(packet: &[u8], id: &[u8; 12]) -> Option<SocketAddr> {
    if packet.len() < 20
        || u16::from_be_bytes([packet[0], packet[1]]) != 0x0101
        || u32::from_be_bytes(packet[4..8].try_into().ok()?) != MAGIC
        || &packet[8..20] != id
    {
        return None;
    }
    let mut rest = &packet[20..];
    let mut plain = None;
    while rest.len() >= 4 {
        let kind = u16::from_be_bytes([rest[0], rest[1]]);
        let len = u16::from_be_bytes([rest[2], rest[3]]) as usize;
        let value = rest.get(4..4 + len)?;
        let xor = kind == 0x0020;
        if (xor || kind == 0x0001) && value.len() >= 8 {
            let mut port = u16::from_be_bytes([value[2], value[3]]);
            let ip = match value[1] {
                1 => {
                    let mut o: [u8; 4] = value[4..8].try_into().ok()?;
                    if xor {
                        port ^= (MAGIC >> 16) as u16;
                        for (b, m) in o.iter_mut().zip(MAGIC.to_be_bytes()) {
                            *b ^= m;
                        }
                    }
                    IpAddr::V4(Ipv4Addr::from(o))
                }
                2 if value.len() >= 20 => {
                    let mut o: [u8; 16] = value[4..20].try_into().ok()?;
                    if xor {
                        port ^= (MAGIC >> 16) as u16;
                        let key: Vec<u8> = MAGIC.to_be_bytes().iter().chain(id).copied().collect();
                        for (b, m) in o.iter_mut().zip(key) {
                            *b ^= m;
                        }
                    }
                    IpAddr::V6(Ipv6Addr::from(o))
                }
                _ => return None,
            };
            let addr = SocketAddr::new(ip, port);
            if xor {
                return Some(addr);
            }
            plain = Some(addr);
        }
        rest = rest.get(4 + len.div_ceil(4) * 4..).unwrap_or(&[]);
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_xor_mapped_address() {
        let id = [7u8; 12];
        let ip = Ipv4Addr::new(85, 105, 1, 2);
        let port: u16 = 41234;
        let mut packet = vec![0x01, 0x01, 0, 12];
        packet.extend_from_slice(&MAGIC.to_be_bytes());
        packet.extend_from_slice(&id);
        packet.extend_from_slice(&[0x00, 0x20, 0, 8, 0, 1]);
        packet.extend_from_slice(&(port ^ (MAGIC >> 16) as u16).to_be_bytes());
        for (b, m) in ip.octets().iter().zip(MAGIC.to_be_bytes()) {
            packet.push(b ^ m);
        }
        assert_eq!(
            parse(&packet, &id),
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        );
        // Başka bir isteğin yanıtı kabul edilmez
        assert_eq!(parse(&packet, &[0u8; 12]), None);
    }
}
