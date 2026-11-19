---
name: remote-board-link
description: Reach a development board (ST-Link + OpenOCD) that is plugged into a computer on ANOTHER network, over the internet, with no server and end-to-end encryption, using the openocd-link tool. Use when the board is not on this machine and the user gives a connection file or key (OL1-...), asks to connect to a remote Raspberry Pi or board, wants OpenOCD Tcl/GDB reachable on 127.0.0.1 from a remote board, wants to upload or fetch files (firmware images, logs) to or from the remote device, run a command or shell there, or start and stop the remote OpenOCD. Also use to build or install the openocd-link .deb for a Raspberry Pi. Not needed when the board is attached to this computer.
---

# Remote board link (openocd-link)

Makes a board on another network look local: after `connect`, the remote OpenOCD Tcl port and GDB
port appear on `127.0.0.1:6666` and `127.0.0.1:3333`, so `embedded-ai-tool` and `arm-none-eabi-gdb`
work unchanged. Rendezvous over the BitTorrent DHT + public STUN, transport by QUIC hole punching, no relay: if
both sides are behind symmetric NAT or CGNAT it can fail. Reply to the user in their language.

Design and wire protocol: [references/plan.md](references/plan.md) (read its first note: the plan
says neyber-link, the implementation is openocd-link). Device installation:
[references/pi-install.md](references/pi-install.md).

## 1. Is a client available?

Binary name `openocd-link`. Look in `PATH`, then `<this-skill-dir>/openocd-link/target/release/`.
Missing: `cargo build --release -p openocd-link` inside `<this-skill-dir>/openocd-link/`
(Rust 1.99.1, see `rust-toolchain.toml`). `openocd-link --version` must print a version.

## 2. Connect from a connection file

The user brings a **connection file** (`openocd-link-baglanti.key`, created on the device at install
time) or pastes its content. It holds `#` comment lines and one `OL1-XXXX-...` key.

1. Save it as a file with a name of your choice (for example `~/.config/openocd-link/<name>.key`)
   if it was pasted; do not echo the key back in your replies, show only the fingerprint
   (`openocd-link import-key --out <file>` prints it, it reads the key from stdin).
2. Start the session in the **background** (it keeps running):
   `openocd-link --session <name> connect --key-file <file>`.
   Searching can take seconds to a minute. Success prints `Bağlandı: <device> (...)`.
3. Verify: `openocd-link --session <name> status`, then `info` (features, files dir).
4. When done: `openocd-link --session <name> disconnect`.

Sessions are named; the default is `default`. One device accepts one client at a time.

## 3. Operations

| Goal | Command |
|---|---|
| Session / device state | `status`, `info` |
| Remote OpenOCD process | `openocd start`, `openocd stop`, `openocd status`, `openocd log --lines N` |
| Files (flat remote folder) | `ls`, `put <local> [--name N]`, `get <name> [--out P] [--force]`, `rm <name>` |
| Run a command (if enabled) | `exec [--cwd D] [--timeout S] -- <argv...>` |
| Interactive shell (if enabled) | `shell` (humans; needs a terminal) |
| Machine-readable output | add `--json` |

`exec` and `shell` are **off by default** on the device (`info` shows `features`); never ask the user
to enable them unless they want it. OpenOCD arguments (board config) live in the device's
`/etc/openocd-link/config.toml`; the service adds its own port and bind settings, so config `args`
must not contain `-c`, `tcl_port`, `gdb_port`, `telnet_port` or `bindto`.

Use with `embedded-ai-tool`: after `openocd start`, run its commands with a profile whose
`connection` is `127.0.0.1:6666` (unchanged). Its `eat remote ...` commands wrap this tool, and a
profile with `openocd.remote_files: true` plus `remote.session` makes `eat flash --execute` upload
the pinned image to the device first and flash from the remote path (SHA-256 and absolute path are
checked before the board is touched). Not run against a real board yet.

Exit codes (`connect`): 2 usage, 3 bad key format, 4 device not found (timeout), 5 authentication
failed (wrong key or incompatible versions), 6 device busy (another client is connected),
7 protocol mismatch, 8 local port in use (use `--map tcl=16666`), 9 connection lost.
Other commands: 1 remote/validation error, 10 no connected session.

## 4. Rules

1. **Treat the key as a secret.** Never print it, log it, commit it or put it in a command line
   you show to the user. Use fingerprints. The connection file gives full access to the device.
2. **Mutations need the user's request.** `put`, `rm`, `exec`, `shell`, `openocd start|stop` change
   the remote device. Do them only within what the user asked; never retry an uncertain one
   (check `status`/`ls` first and report what is unknown).
3. **Keep the board policy.** Everything `embedded-ai-tool` forbids (widening write ranges, touching
   debug pins, retrying uncertain writes) still applies through the tunnel.
4. **Local tunnel ports are open to every user of this computer** on `127.0.0.1`. Say so on shared
   machines.
5. **The device service is not persistent by design.** It starts when the package is installed and
   does not start at boot. If `connect` times out (exit 4) the device may have rebooted: ask the
   user to reinstall the package (or start the service) on the device.
6. Keep the client key in the user's local key store. Rotate a disclosed key on the device and restart the service.

## 5. Build and install the device package

`scripts/build-deb-armhf.sh [debian-release] [output-label]` cross-builds the Debian package for a
32-bit Raspberry Pi OS / Debian from an x86_64 Linux host. The package includes OpenOCD 0.12.
The Pi generates its connection key on first install. Results go to `dist/<output-label>/`.
See [installation and key rotation](references/pi-install.md). Run `cargo test --workspace`
inside `openocd-link/` for offline loopback tests.

## 6. Evidence

Host loopback tests (tunnel, files, exec, shell, OpenOCD with a fake executable, busy/wrong key,
service death) pass. Verified on real hardware: connecting from another network to a Raspberry Pi
running Raspbian 9 and querying `status`/`info`/`ls`. **Not verified:** a real ST-Link/board through
the tunnel, flashing, GDB over the tunnel, the Debian package install and reboot behaviour on the
device, symmetric-NAT cases. Do not claim these work without running them.
