#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
[ $# -ge 1 ] || { echo "usage: tools/restore-keys.sh BACKUP.asc [KEYS_DIR] [--check-only]"; exit 2; }
FILE="$1"
KEYS="$HOME/svx-keys"
CHECK=0
for arg in "${@:2}"; do
	case "$arg" in
		--check-only) CHECK=1 ;;
		*) KEYS="$arg" ;;
	esac
done
command -v gpg >/dev/null || { echo "gpg is required (Linux: gnupg package, Windows: Gpg4win)"; exit 1; }
case "$(realpath -m "$KEYS")/" in "$ROOT"/*) echo "keep keys outside the repository: $KEYS"; exit 1 ;; esac

tools/cargo.sh build -q -p svx-release
REL="${CARGO_TARGET_DIR:-$ROOT/target}/debug/svx-release"
SCRATCH="$(mktemp -d -p "$([ -d /dev/shm ] && echo /dev/shm || echo "${TMPDIR:-/tmp}")")"
trap 'rm -rf "$SCRATCH"' EXIT
gpg --quiet --no-symkey-cache --decrypt --output "$SCRATCH/keys.tar" "$FILE"
tar -C "$SCRATCH" -xf "$SCRATCH/keys.tar"
SRC="$SCRATCH/svx-keys"
"$REL" check-keys --root trust/production/1.root.json --roots trust/production --key "$SRC/root.key" --key "$SRC/release.key"
if [ "$CHECK" = 1 ]; then
	echo "backup is intact and matches trust/production"
	exit 0
fi
mkdir -p "$KEYS"
chmod 700 "$KEYS"
for f in root.key root.pub release.key release.pub; do
	if [ -f "$KEYS/$f" ] && ! cmp -s "$KEYS/$f" "$SRC/$f"; then
		echo "$KEYS/$f exists with different content; move it away first"
		exit 1
	fi
done
for f in root.key root.pub release.key release.pub; do
	install -m "$([ "${f##*.}" = key ] && echo 600 || echo 644)" "$SRC/$f" "$KEYS/$f"
done
echo "restored keys to $KEYS"
