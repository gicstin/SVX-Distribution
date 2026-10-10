#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
export PATH="$HOME/.cargo/bin:$PATH"
ZIG_DIR="${ZIG_DIR:-$HOME/.local/opt/zig-x86_64-linux-0.15.2}"
[ -d "$ZIG_DIR" ] && export PATH="$ZIG_DIR:$PATH"

usage() {
	echo "usage: tools/build.sh [--test] [--linux-only|--windows-only]"
	echo "  release build: SVX_TRUSTED_ROOT defaults to trust/production/1.root.json and must not be the test root"
	echo "  --test embeds trust/test/1.root.json (publicly known keys) and allows a local index; never distribute it"
	echo "  output: out/patcher/<version>/{svx-patcher,SVXPatcher.exe,SHA256SUMS}"
}

TEST=0
LINUX=1
WINDOWS=1
for arg in "$@"; do
	case "$arg" in
		--test) TEST=1 ;;
		--linux-only) WINDOWS=0 ;;
		--windows-only) LINUX=0 ;;
		-h|--help) usage; exit 0 ;;
		*) usage; exit 2 ;;
	esac
done

if [ "$TEST" = 1 ]; then
	export SVX_TRUSTED_ROOT="$ROOT/trust/test/1.root.json"
	unset SVX_RELEASE_BUILD
	SUFFIX="-test"
else
	export SVX_TRUSTED_ROOT="${SVX_TRUSTED_ROOT:-$ROOT/trust/production/1.root.json}"
	[ -f "$SVX_TRUSTED_ROOT" ] || { echo "missing $SVX_TRUSTED_ROOT; create the production root first, or pass --test"; exit 1; }
	export SVX_RELEASE_BUILD=1
	SUFFIX=""
fi
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 0)}"
export CARGO_INCREMENTAL=0
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$ROOT=/svx-distribution --remap-path-prefix=$HOME=/home"

VERSION="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="svx-patcher"))')"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
OUT="$ROOT/out/patcher/$VERSION$SUFFIX"
rm -rf "$OUT"
mkdir -p "$OUT"

if [ "$LINUX" = 1 ]; then
	if command -v cargo-zigbuild >/dev/null && command -v zig >/dev/null; then
		cargo zigbuild --locked --release -p svx-patcher --target x86_64-unknown-linux-gnu.2.28
	else
		echo "cargo-zigbuild and zig are needed for a portable Linux build (glibc 2.28); see tools/build.sh"
		exit 1
	fi
	cp "$TARGET_DIR/x86_64-unknown-linux-gnu/release/svx-patcher" "$OUT/svx-patcher"
	MAX_GLIBC="$(objdump -T "$OUT/svx-patcher" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
	echo "linux: needs $MAX_GLIBC"
fi
if [ "$WINDOWS" = 1 ]; then
	cargo build --locked --release -p svx-patcher --target x86_64-pc-windows-gnu
	cp "$TARGET_DIR/x86_64-pc-windows-gnu/release/svx-patcher.exe" "$OUT/SVXPatcher.exe"
fi
(cd "$OUT" && sha256sum svx-patcher SVXPatcher.exe 2>/dev/null > SHA256SUMS || true)
cat "$OUT/SHA256SUMS"
ls -la "$OUT"
