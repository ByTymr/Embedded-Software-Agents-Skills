#!/bin/sh
# openocd-link servisini kurar. Root olarak çalışır ve tekrar çalıştırılabilir:
# ikinci çalıştırma anahtarı ve yapılandırmayı değiştirmez.
# Kullanım: sudo ./install-link.sh ./openocd-link
set -eu

BIN="${1:-}"
HERE="$(cd "$(dirname "$0")" && pwd)"

if [ "$(id -u)" -ne 0 ]; then
    echo "Root olarak çalıştırın: sudo $0 <openocd-link ikilisi>" >&2
    exit 1
fi
if [ -z "$BIN" ] || [ ! -f "$BIN" ]; then
    echo "Kullanım: $0 <openocd-link ikilisi>" >&2
    exit 1
fi

install -m 0755 "$BIN" /usr/local/bin/openocd-link

if ! id openocd-link >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/openocd-link --shell /usr/sbin/nologin openocd-link
fi
if getent group plugdev >/dev/null 2>&1; then
    usermod -a -G plugdev openocd-link
else
    echo "ST-Link erişimi için OpenOCD udev kurallarını kontrol edin"
fi

install -d -m 0750 -o root -g openocd-link /etc/openocd-link
if [ ! -f /etc/openocd-link/config.toml ]; then
    install -m 0640 -o root -g openocd-link "$HERE/openocd-link.toml" /etc/openocd-link/config.toml
fi

if [ ! -f /etc/openocd-link/key ]; then
    /usr/local/bin/openocd-link keygen --out /etc/openocd-link/key
    chown openocd-link:openocd-link /etc/openocd-link/key
    chmod 600 /etc/openocd-link/key
    echo "Yukarıdaki bağlantı anahtarı bir kez gösterilir; güvenli bir yere kaydedin."
fi

install -m 0644 "$HERE/openocd-link.service" /etc/systemd/system/openocd-link.service
systemctl daemon-reload
systemctl enable --now openocd-link

echo "Yapılandırmadaki [openocd].args değerini kartınıza göre düzenleyin, ardından: systemctl restart openocd-link"
