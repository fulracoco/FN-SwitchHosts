#!/bin/bash
# Stage on a native Linux filesystem so FPK executables retain Unix modes,
# including when the source checkout lives on a Windows/WSL mount.
set -euo pipefail
umask 022
[ "$(uname -s)" = Linux ] || { echo 'Build the FPK on Linux (WSL is supported).' >&2; exit 1; }
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target="${SWH_TARGET:-x86_64-unknown-linux-gnu}"
case "$target" in
    x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl) platform=x86; machine='Advanced Micro Devices X86-64' ;;
    aarch64-unknown-linux-gnu|aarch64-unknown-linux-musl) platform=arm; machine=AArch64 ;;
    *) echo "Unsupported Linux target: $target" >&2; exit 1 ;;
esac
cd "$repo"
if [ -z "${SWH_BINARY:-}" ]; then
    cargo build --release --locked --manifest-path fnos/server/Cargo.toml --target "$target"
fi
binary="${SWH_BINARY:-${CARGO_TARGET_DIR:-${repo}/fnos/server/target}/${target}/release/switchhosts-fnos}"
binary="$(readlink -f "$binary")"
[ -f "$binary" ] || { echo 'Linux service binary is missing.' >&2; exit 1; }
header="$(LC_ALL=C readelf -h "$binary")"
[[ "$header" == *ELF64* && "$header" == *"2's complement, little endian"* && "$header" == *"$machine"* ]] || { echo 'Binary is not a matching 64-bit little-endian Linux ELF.' >&2; exit 1; }
stage="$(mktemp -d /var/tmp/swh-fnos-build.XXXXXX)"
cleanup() {
    case "$(readlink -f "$stage")" in /var/tmp/swh-fnos-build.*) rm -rf -- "$stage" ;; esac
}
trap cleanup EXIT
cp -R "$repo/fnos/package/." "$stage/"
mkdir -p "$stage/app/server" "$stage/app/ui/images"
cp "$binary" "$stage/app/server/switchhosts-fnos"
sed -i "s/^platform[[:space:]]*=.*/platform = ${platform}/" "$stage/manifest"
cp "$repo/src-tauri/icons/64x64.png" "$stage/ICON.PNG"
cp "$repo/src-tauri/icons/128x128@2x.png" "$stage/ICON_256.PNG"
cp "$stage/ICON.PNG" "$stage/app/ui/images/icon_64.png"
cp "$stage/ICON_256.PNG" "$stage/app/ui/images/icon_256.png"
find "$stage" -type d -exec chmod 0755 {} +
find "$stage" -type f -exec chmod 0644 {} +
chmod 0755 "$stage"/cmd/* "$stage/app/server/switchhosts-fnos"
output="$repo/dist/fnos/$target"
mkdir -p "$output"
# fnpack 1.2.3 may return zero even after a packaging error. Pack in a new
# directory and require its actual output before replacing the prior artifact.
(cd "$stage" && "${FNPACK:-fnpack}" build --directory "$stage")
[ -s "$stage/switchhosts.fpk" ] || { echo 'fnpack did not produce an FPK.' >&2; exit 1; }
cp "$stage/switchhosts.fpk" "$output/FN-SwitchHosts.fpk"
printf 'FPK: %s\n' "$output/FN-SwitchHosts.fpk"
