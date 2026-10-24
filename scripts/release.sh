#!/usr/bin/env bash
# Build the release artifacts into dist/:
#   studio-<v>-linux-x86_64.tar.gz           dynamic build (needs this host's glibc or newer)
#   studio-<v>-linux-x86_64-portable.tar.gz  glibc 2.31 build (Ubuntu 20.04+/Debian 11+),
#                                             all Rust code and libgcc linked statically
#   studio-<v>-windows-x86_64-setup.exe      NSIS installer
#   SHA256SUMS
#
# Requirements: rustup targets x86_64-pc-windows-gnu; mingw-w64; nsis (makensis);
# cargo-zigbuild with zig on PATH or ZIG_DIR (default ~/.local/share/studio-build/zig).
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ZIG_DIR=${ZIG_DIR:-$HOME/.local/share/studio-build/zig}
export PATH="$ZIG_DIR:$PATH"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-3}
DIST=dist
rm -rf "$DIST" && mkdir -p "$DIST"

echo "== Studio $VERSION"
cargo build --release
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31
cargo build --release --target x86_64-pc-windows-gnu

package_linux() { # <binary> <name>
    local dir="$DIST/$2"
    mkdir -p "$dir"
    install -m 755 "$1" "$dir/studio"
    cp LICENSE README.md DEPENDENCIES.md "$dir/"
    tar -C "$DIST" -czf "$DIST/$2.tar.gz" "$2"
    rm -rf "$dir"
}
package_linux target/release/studio "studio-$VERSION-linux-x86_64"
package_linux target/x86_64-unknown-linux-gnu/release/studio "studio-$VERSION-linux-x86_64-portable"

makensis -V2 -DVERSION="$VERSION" \
    -DEXE="$(pwd)/target/x86_64-pc-windows-gnu/release/studio.exe" \
    -DOUTFILE="$(pwd)/$DIST/studio-$VERSION-windows-x86_64-setup.exe" \
    packaging/windows/studio.nsi

(cd "$DIST" && sha256sum -- * > SHA256SUMS)
ls -la "$DIST"
