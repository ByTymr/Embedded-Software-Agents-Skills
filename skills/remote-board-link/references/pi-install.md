# Installing openocd-link on a Raspberry Pi

## Package

Build: `skills/remote-board-link/scripts/build-deb-armhf.sh stretch lab` (use the Debian release the
Pi runs: `cat /etc/os-release`; Raspbian 9 = stretch, glibc 2.24). Result:
`dist/lab/openocd-link_<version>_armhf.deb`. `uname -m` on the Pi must be
`armv7l` (32-bit); a 64-bit Pi OS needs an aarch64 build (not scripted).

Install on the Pi (the user does this once, no other command):

```
sudo apt install ./openocd-link_<version>_armhf.deb
```

The package ships its own **OpenOCD 0.12** (cross-built, `/usr/lib/openocd-link/bin/openocd`) because the Raspberry Pi GPIO driver needs SWD support (OpenOCD 0.10+; Debian 9 has 0.9). It depends only on `adduser`, `libc6`, `libgcc1 | libgcc-s1`.
Wrong-architecture files (`amd64`) fail with "libc6:amd64 not installable".

## What the install does

1. Creates the system user `openocd-link`, adds it to `plugdev` (ST-Link access).
2. Writes `/etc/openocd-link/config.toml` and `/etc/openocd-link/raspberrypi-swd.cfg` (SWD over the Pi GPIO header: SWCLK = GPIO25, SWDIO = GPIO24, target STM32F730V8 (64 KB) via `stm32f730v8.cfg`; the install sets `peripheral_base` from the Pi model). **Edit them for the real wiring/chip**, then `sudo systemctl restart openocd-link`. For an ST-Link instead, use `-f interface/stlink.cfg` (ST-Link V3 and OpenOCD 0.12: fine).
3. A missing or empty device key is generated on the Pi at `/etc/openocd-link/key` (mode 0600). Upgrades preserve the existing key and configuration. The package's `CONNECT_TO` build setting only applies when its default configuration is first installed. Set the fixed endpoint on an existing Pi with `sudo openocd-link set-client <client-ip>:47124` or update `connect_to` in its configuration.
4. Transfer the key file through a trusted channel, then open it locally with `openocd-link import-key --out ~/.config/openocd-link/lab.key < /path/to/received.key`. If the device used a package containing a key, rotate it on the Pi with `sudo openocd-link keygen --force --out /etc/openocd-link/key`, restore ownership with `sudo chown openocd-link:openocd-link /etc/openocd-link/key`, restart the service and import the new key on the client.
5. Starts the service during installation, as **root** (the Pi GPIO driver opens
   `/dev/mem`). systemd restarts it five seconds after a failure. After a reboot,
   start it with `sudo systemctl start openocd-link`.

## Troubleshooting

- `Unknown lvalue 'StateDirectory'` in the journal on Debian 9: harmless (old systemd 232).
- `deb-systemd-helper: error: unable to read openocd-link.service` during install: harmless on
  Debian 9; the service still starts.
- Journal: `journalctl -u openocd-link -n 30` shows the key fingerprint and `Kayıt DHT'ye yazıldı`
  (the device announced itself). No such line: the device has no internet or DHT access.
- ST-Link not seen: `lsusb` must show `0483:...`; the service user must be in `plugdev`.
- Debian 9 is end-of-life: `apt` may need archive mirrors in `/etc/apt/sources.list`.

## Network limits (no relay)

The Pi measures its own NAT; when the NAT changes ports per destination (symmetric, typical for
mobile/CGNAT links) it publishes a predicted port range and the client dials that range too.
This is a prediction, not a guarantee. If both sides are behind symmetric NAT/CGNAT, a direct
connection may still fail; the fixes are a different network on one side, a UDP port forward of
`udp_port` (47123) on the Pi's router, or IPv6.
