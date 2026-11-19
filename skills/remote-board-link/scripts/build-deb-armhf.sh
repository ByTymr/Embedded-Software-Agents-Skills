#!/bin/sh
# Builds openocd-link_<version>_armhf.deb for a Raspberry Pi (32-bit Raspbian/Debian) from any
# x86_64 Linux host, without sudo and without running downloaded programs.
#
# It links against the glibc of the TARGET Debian release (downloaded as plain .deb data and
# verified with the SHA-256 from that release's Packages index), so the binary runs on that
# release and newer. Default release: stretch (Debian 9, glibc 2.24).
#
# Needs: clang, gcc, make, rustup/cargo, cargo-deb (cargo install cargo-deb), curl, python3, dpkg-deb.
# The package also ships OpenOCD 0.12 (Raspberry Pi GPIO/SWD driver), cross-built from the official
# release tarball whose SHA-256 is pinned below.
# Usage: scripts/build-deb-armhf.sh [debian-release] [output-label]
# CONNECT_TO=ip:port optionally sets a fixed client endpoint in the package defaults.
# Output: dist/<output-label>/openocd-link_<ver>_armhf.deb
set -eu

RELEASE="${1:-stretch}"
DEVICE="${2:-pi}"
case "$DEVICE" in *[!a-z0-9_-]*|"") echo "Cihaz adı [a-z0-9_-] olmalı" >&2; exit 1 ;; esac
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$HERE/../openocd-link"
WORK="${WORK:-${TMPDIR:-/tmp}/openocd-link-sysroot-$RELEASE}"
TARGET=armv7-unknown-linux-gnueabihf

case "$RELEASE" in
    stretch | buster) MIRROR=https://archive.debian.org/debian ;;
    *) MIRROR=https://deb.debian.org/debian ;;
esac

for tool in clang gcc make cargo cargo-deb curl python3 dpkg-deb sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || { echo "Eksik araç: $tool" >&2; exit 1; }
done

mkdir -p "$WORK"
if [ ! -d "$WORK/sysroot/usr/lib" ]; then
    curl -fsSL -o "$WORK/Packages.xz" "$MIRROR/dists/$RELEASE/main/binary-armhf/Packages.xz"
    python3 -I - "$WORK" "$MIRROR" <<'PY'
import hashlib, lzma, re, subprocess, sys, urllib.request
work, mirror = sys.argv[1], sys.argv[2]
index = lzma.open(work + "/Packages.xz", "rt", encoding="utf-8").read().split("\n\n")
pkgs = {}
for block in index:
    d = dict(re.findall(r"^([A-Za-z0-9.+-]+): (.*)$", block, re.M))
    if "Package" in d:
        pkgs[d["Package"]] = d
gcc = sorted({m.group(1) for n in pkgs if (m := re.fullmatch(r"libgcc-(\d+)-dev", n))}, key=int)[-1]
want = ["libc6", "libc6-dev", "linux-libc-dev", f"libgcc-{gcc}-dev", f"gcc-{gcc}-base"]
want += ["libgcc-s1" if "libgcc-s1" in pkgs else "libgcc1"]
want += [n for n in ("libcrypt1", "libcrypt-dev") if n in pkgs]
subprocess.check_call(["mkdir", "-p", work + "/debs", work + "/sysroot"])
for name in want:
    d = pkgs[name]
    data = urllib.request.urlopen(mirror + "/" + d["Filename"]).read()
    if hashlib.sha256(data).hexdigest() != d["SHA256"]:
        sys.exit("SHA-256 tutmadı: " + name)
    path = f"{work}/debs/{name}.deb"
    open(path, "wb").write(data)
    subprocess.check_call(["dpkg-deb", "-x", path, work + "/sysroot"])
open(work + "/gcc-version", "w").write(gcc)
PY
    # Mutlak sembolik bağları sysroot içine çevir
    find "$WORK/sysroot" -type l | while read -r link; do
        target="$(readlink "$link")"
        case "$target" in /*) ln -sf "$WORK/sysroot$target" "$link" ;; esac
    done
fi

GCC="$(cat "$WORK/gcc-version")"
TOOLCHAIN_BIN="$(cd "$SRC" && rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin"
cat > "$WORK/cc-armhf.sh" <<WRAP
#!/bin/sh
exec clang --target=armv7-linux-gnueabihf --sysroot=$WORK/sysroot --gcc-install-dir=$WORK/sysroot/usr/lib/gcc/arm-linux-gnueabihf/$GCC -fuse-ld=lld -B$TOOLCHAIN_BIN/gcc-ld "\$@"
WRAP
chmod +x "$WORK/cc-armhf.sh"

# Pakete dahil OpenOCD (Pi GPIO/SWD sürücüsü 0.10+ ister; Debian 9 deposunda 0.9 var)
OPENOCD_VER=0.12.0
OPENOCD_SHA=af254788be98861f2bd9103fe6e60a774ec96a8c374744eef9197f6043075afa
STAGE="$SRC/packaging/linux/openocd-stage"
if [ ! -x "$STAGE/usr/lib/openocd-link/bin/openocd" ]; then
    TARBALL="$WORK/openocd_$OPENOCD_VER.orig.tar.bz2"
    [ -f "$TARBALL" ] || curl -fsSL -o "$TARBALL" \
        "https://deb.debian.org/debian/pool/main/o/openocd/openocd_$OPENOCD_VER.orig.tar.bz2"
    echo "$OPENOCD_SHA  $TARBALL" | sha256sum -c - >/dev/null || { echo "OpenOCD SHA-256 tutmadı" >&2; exit 1; }
    rm -rf "$WORK/openocd-src" "$WORK/openocd-build" "$STAGE"
    mkdir -p "$WORK/openocd-src" "$WORK/openocd-build"
    tar xjf "$TARBALL" -C "$WORK/openocd-src"
    (
        cd "$WORK/openocd-build"
        PKG_CONFIG_LIBDIR=/nonexistent PKG_CONFIG_PATH= CC="$WORK/cc-armhf.sh" CC_FOR_BUILD=gcc \
            "$WORK/openocd-src/openocd-$OPENOCD_VER/configure" --build=x86_64-linux-gnu \
            --host=arm-linux-gnueabihf --prefix=/usr/lib/openocd-link --disable-werror \
            --disable-doxygen-html --disable-doxygen-pdf --enable-bcm2835gpio --enable-sysfsgpio \
            --enable-internal-jimtcl --disable-internal-libjaylink >"$WORK/openocd-configure.log" 2>&1
        make -j"$(nproc)" >"$WORK/openocd-make.log" 2>&1
        make install DESTDIR="$STAGE" >>"$WORK/openocd-make.log" 2>&1
    )
    for strip_tool in llvm-strip llvm-strip-18 llvm-strip-19 llvm-strip-20 arm-none-eabi-strip; do
        if command -v "$strip_tool" >/dev/null 2>&1; then
            "$strip_tool" --strip-all "$STAGE/usr/lib/openocd-link/bin/openocd" && break
        fi
    done
fi

cd "$SRC"
rustup target add "$TARGET" >/dev/null
export CC_armv7_unknown_linux_gnueabihf="$WORK/cc-armhf.sh"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="$WORK/cc-armhf.sh"
TOML="$SRC/packaging/linux/openocd-link.toml"
cp "$TOML" "$WORK/openocd-link.toml.orig"
trap 'cp "$WORK/openocd-link.toml.orig" "$TOML"' EXIT
if [ -n "${CONNECT_TO:-}" ]; then
    sed -i "s|^connect_to = \[\]|connect_to = [\"$CONNECT_TO\"]|" "$TOML"
    grep -q "^connect_to = \[\"$CONNECT_TO\"\]" "$TOML" || { echo "connect_to yazılamadı" >&2; exit 1; }
fi
cargo build --release --locked -p openocd-link --target "$TARGET"
cargo deb -p openocd-link --no-build --no-strip --target "$TARGET"
DEB="$(ls -1t "$SRC"/target/debian/*_armhf.deb | head -1)"
DIST="$HERE/../dist/$DEVICE"
mkdir -p "$DIST"
cp "$DEB" "$DIST/"
echo "$DIST/$(basename "$DEB")"
