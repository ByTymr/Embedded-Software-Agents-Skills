//! Modemde port açma (UPnP IGD). Dış servis yok: yalnız yerel ağdaki modemle konuşulur.
//! IPv4 için port yönlendirmesi, IPv6 için modem güvenlik duvarında geçiş (pinhole) istenir;
//! modem dış IPv4 adresini de bildirir. Bekleme bitince açılanlar geri kapatılır.
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const LEASE: u32 = 3600;
const RENEW: Duration = Duration::from_secs(1500);
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Default)]
pub struct Status {
    /// Kullanıcıya gösterilen tek satır
    pub note: String,
    /// Dışarıdan bağlanılacak IPv4 adresi (port yönlendirmesi kurulduysa)
    pub external: Option<SocketAddr>,
    pub done: bool,
}

pub struct Mapping {
    pub status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Arka planda açar, süre dolmadan yeniler; `Mapping` bırakılınca kapatır.
pub fn open(port: u16, v4: Option<Ipv4Addr>, v6: Option<Ipv6Addr>) -> Mapping {
    let status = Arc::new(Mutex::new(Status {
        note: "Modemde port açılıyor…".into(),
        ..Status::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let (s, st) = (status.clone(), stop.clone());
    thread::spawn(move || run(port, v4, v6, &s, &st));
    Mapping { status, stop }
}

fn run(
    port: u16,
    v4: Option<Ipv4Addr>,
    v6: Option<Ipv6Addr>,
    status: &Mutex<Status>,
    stop: &AtomicBool,
) {
    let set = |note: String, external: Option<SocketAddr>| {
        let mut s = status.lock().unwrap();
        s.note = note;
        s.external = external;
        s.done = true;
    };
    let Some(gateway) = discover() else {
        set(
            "Modem UPnP'ye yanıt vermedi. Modem ayarlarında UPnP'yi açın ya da portu elle yönlendirin.".into(),
            None,
        );
        return;
    };
    let mut notes = Vec::new();
    let mut external = None;
    let mut mapped = false;
    let mut pinhole = Vec::new();
    if let (Some(v4), Some(wan)) = (v4, &gateway.wan) {
        match add_port_mapping(wan, port, v4) {
            Ok(()) => {
                mapped = true;
                match external_ip(wan) {
                    Ok(ip) if is_shared(ip) => notes.push(format!(
                        "IPv4 yönlendirildi ama modemin dış adresi {ip} paylaşımlı (CGNAT): internetten IPv4 ile ulaşılamaz."
                    )),
                    Ok(ip) => {
                        external = Some(SocketAddr::new(IpAddr::V4(ip), port));
                        notes.push(format!("IPv4 açık: {ip}:{port}"));
                    }
                    Err(_) => notes.push("IPv4 yönlendirildi; dış adres öğrenilemedi.".into()),
                }
            }
            Err(e) => notes.push(format!("IPv4 yönlendirilemedi ({e}).")),
        }
    }
    if let (Some(v6), Some(fw)) = (v6, &gateway.firewall) {
        if let Ok(id) = add_pinhole(fw, port, v6, "17") {
            pinhole.push(id);
        }
        match add_pinhole(fw, port, v6, "6") {
            Ok(id) => {
                pinhole.push(id);
                notes.push("IPv6 kapısı açık.".into());
            }
            Err(e) => notes.push(format!("IPv6 kapısı açılamadı ({e}).")),
        }
    } else if v6.is_some() {
        notes.push("Modem IPv6 kapısı açmayı desteklemiyor.".into());
    }
    set(notes.join(" "), external);
    let mut renewed = Instant::now();
    while !stop.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(200));
        if renewed.elapsed() >= RENEW {
            renewed = Instant::now();
            if let (true, Some(v4), Some(wan)) = (mapped, v4, &gateway.wan) {
                let _ = add_port_mapping(wan, port, v4);
            }
            if let Some(fw) = &gateway.firewall {
                for id in &pinhole {
                    let _ = soap(
                        fw,
                        "UpdatePinhole",
                        &[("UniqueID", id), ("NewLeaseTime", &LEASE.to_string())],
                    );
                }
            }
        }
    }
    if let (true, Some(wan)) = (mapped, &gateway.wan) {
        for protocol in ["TCP", "UDP"] {
            let _ = soap(
                wan,
                "DeletePortMapping",
                &[
                    ("NewRemoteHost", ""),
                    ("NewExternalPort", &port.to_string()),
                    ("NewProtocol", protocol),
                ],
            );
        }
    }
    if let Some(fw) = &gateway.firewall {
        for id in &pinhole {
            let _ = soap(fw, "DeletePinhole", &[("UniqueID", id)]);
        }
    }
}

/// CGNAT (100.64/10) ya da özel adres: modemin kendisi de bir NAT arkasında
fn is_shared(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_unspecified()
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
}

struct Service {
    host: SocketAddr,
    path: String,
    kind: String,
}

struct Gateway {
    wan: Option<Service>,
    firewall: Option<Service>,
}

fn discover() -> Option<Gateway> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    for st in [
        "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    ] {
        let request = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
        );
        let _ = socket.send_to(request.as_bytes(), "239.255.255.250:1900");
    }
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut buf = [0u8; 2048];
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let text = String::from_utf8_lossy(&buf[..n]);
        let Some(location) = header(&text, "location") else {
            continue;
        };
        if seen.contains(&location) {
            continue;
        }
        seen.push(location.clone());
        if let Some(gateway) = describe(&location)
            && (gateway.wan.is_some() || gateway.firewall.is_some())
        {
            return Some(gateway);
        }
    }
    None
}

fn header(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_owned())
    })
}

/// "http://192.168.1.1:5000/desc.xml" → (adres, yol)
fn split_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let host: SocketAddr = host.parse().ok()?;
    // Yalnız yerel ağdaki modem: SSDP yanıtı başka bir yere yönlendiremez
    match host.ip() {
        IpAddr::V4(ip) if ip.is_private() || ip.is_link_local() => {}
        _ => return None,
    }
    Some((
        host,
        if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
    ))
}

fn describe(location: &str) -> Option<Gateway> {
    let (host, path) = split_url(location)?;
    let xml = http(
        host,
        &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .ok()?;
    let mut gateway = Gateway {
        wan: None,
        firewall: None,
    };
    for block in xml.split("<service>").skip(1) {
        let (Some(kind), Some(control)) = (tag(block, "serviceType"), tag(block, "controlURL"))
        else {
            continue;
        };
        let path = if let Some(full) = control.strip_prefix("http://") {
            split_url(&format!("http://{full}"))?.1
        } else if control.starts_with('/') {
            control.to_owned()
        } else {
            format!("/{control}")
        };
        let service = Service {
            host,
            path,
            kind: kind.to_owned(),
        };
        if kind.contains(":WANIPConnection:") || kind.contains(":WANPPPConnection:") {
            gateway.wan.get_or_insert(service);
        } else if kind.contains(":WANIPv6FirewallControl:") {
            gateway.firewall.get_or_insert(service);
        }
    }
    Some(gateway)
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(xml[start..start + end].trim())
}

/// TCP (doğrudan bağlantı) ve UDP (QUIC); sonuç TCP'nindir, UDP ek şanstır
fn add_port_mapping(wan: &Service, port: u16, client: Ipv4Addr) -> io::Result<()> {
    let udp = add_one_mapping(wan, port, client, "UDP");
    add_one_mapping(wan, port, client, "TCP").or(udp)
}

fn add_one_mapping(wan: &Service, port: u16, client: Ipv4Addr, protocol: &str) -> io::Result<()> {
    let port = port.to_string();
    let client = client.to_string();
    let args = |lease: &str| {
        [
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", port.clone()),
            ("NewProtocol", protocol.into()),
            ("NewInternalPort", port.clone()),
            ("NewInternalClient", client.clone()),
            ("NewEnabled", "1".into()),
            ("NewPortMappingDescription", "openocd-link".into()),
            ("NewLeaseDuration", lease.into()),
        ]
    };
    let call = |lease: &str| {
        let a = args(lease);
        let refs: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
        soap(wan, "AddPortMapping", &refs)
    };
    // Bazı modemler yalnız süresiz kaydı kabul eder (hata 725); kapatmayı bekleme sonunda yaparız
    call(&LEASE.to_string()).or_else(|_| call("0")).map(drop)
}

fn external_ip(wan: &Service) -> io::Result<Ipv4Addr> {
    let reply = soap(wan, "GetExternalIPAddress", &[])?;
    tag(&reply, "NewExternalIPAddress")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("yanıt anlaşılamadı"))
}

/// IANA protokol numarası: 6 = TCP, 17 = UDP
fn add_pinhole(fw: &Service, port: u16, client: Ipv6Addr, protocol: &str) -> io::Result<String> {
    let reply = soap(
        fw,
        "AddPinhole",
        &[
            ("RemoteHost", ""),
            ("RemotePort", "0"),
            ("InternalClient", &client.to_string()),
            ("InternalPort", &port.to_string()),
            ("Protocol", protocol),
            ("LeaseTime", &LEASE.to_string()),
        ],
    )?;
    tag(&reply, "UniqueID")
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("yanıt anlaşılamadı"))
}

fn soap(service: &Service, action: &str, args: &[(&str, &str)]) -> io::Result<String> {
    let body_args: String = args
        .iter()
        .map(|(k, v)| format!("<{k}>{}</{k}>", escape(v)))
        .collect();
    let body = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{kind}\">{body_args}</u:{action}></s:Body></s:Envelope>",
        kind = service.kind
    );
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{kind}#{action}\"\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        path = service.path,
        host = service.host,
        kind = service.kind,
        len = body.len()
    );
    let reply = http(service.host, &request)?;
    if reply.contains("<s:Fault>") || reply.contains(":Fault>") || reply.contains("<Fault>") {
        let code = tag(&reply, "errorCode").unwrap_or("?");
        let text = tag(&reply, "errorDescription").unwrap_or("modem reddetti");
        return Err(io::Error::other(format!("{text}, kod {code}")));
    }
    Ok(reply)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Tek istekli HTTP/1.1; gövdeyi döndürür (chunked dahil). En çok 256 KB okunur.
fn http(host: SocketAddr, request: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&host, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.take(256 * 1024).read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    Ok(if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    })
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16) else {
            break;
        };
        if n == 0 || rest.len() < n {
            out.push_str(&rest[..n.min(rest.len())]);
            break;
        }
        out.push_str(&rest[..n]);
        body = rest[n..].strip_prefix("\r\n").unwrap_or(&rest[n..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_gateway_services_and_soap_replies() {
        let xml = "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IPConn</controlURL></service></root>";
        let block = xml.split("<service>").nth(1).unwrap();
        assert_eq!(tag(block, "controlURL"), Some("/ctl/IPConn"));
        assert_eq!(dechunk("4\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n"), "abcdefg");
        assert!(is_shared("100.72.1.2".parse().unwrap()));
        assert!(!is_shared("85.105.1.2".parse().unwrap()));
        // Modem dışını işaret eden SSDP yanıtı reddedilir
        assert!(split_url("http://8.8.8.8:80/x").is_none());
        assert_eq!(
            split_url("http://192.168.1.1:5000/d.xml").unwrap().1,
            "/d.xml"
        );
    }
}
