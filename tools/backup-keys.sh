#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
KEYS="${1:-$HOME/svx-keys}"
OUT="${2:-$HOME/svx-keys-backup}"
command -v gpg >/dev/null || { echo "gpg is required (Linux: gnupg package, Windows: Gpg4win)"; exit 1; }
for name in root release; do
	[ -f "$KEYS/$name.key" ] || { echo "missing $KEYS/$name.key"; exit 1; }
done
case "$(realpath -m "$OUT")/" in "$ROOT"/*) echo "write the backup outside the repository: $OUT"; exit 1 ;; esac

tools/cargo.sh build -q -p svx-release
REL="${CARGO_TARGET_DIR:-$ROOT/target}/debug/svx-release"
"$REL" check-keys --root trust/production/1.root.json --roots trust/production --key "$KEYS/root.key" --key "$KEYS/release.key"

SCRATCH="$(mktemp -d -p "$([ -d /dev/shm ] && echo /dev/shm || echo "${TMPDIR:-/tmp}")")"
trap 'rm -rf "$SCRATCH"' EXIT
mkdir -p "$SCRATCH/svx-keys" "$SCRATCH/check"
cp "$KEYS/root.key" "$KEYS/root.pub" "$KEYS/release.key" "$KEYS/release.pub" "$SCRATCH/svx-keys/"
cp trust/production/*.root.json "$SCRATCH/svx-keys/"
tar -C "$SCRATCH" -cf "$SCRATCH/keys.tar" svx-keys

mkdir -p "$OUT"
FILE="$OUT/svx-keys-$(date -u +%Y%m%d-%H%M%S).asc"
echo "choose a long passphrase (6+ random words) and save it in your password manager"
gpg --yes --no-symkey-cache --symmetric --armor --cipher-algo AES256 --s2k-mode 3 --s2k-digest-algo SHA512 --s2k-count 65011712 --output "$FILE" "$SCRATCH/keys.tar"

echo "enter the passphrase again to prove the backup opens"
gpg --quiet --no-symkey-cache --decrypt --output "$SCRATCH/check.tar" "$FILE"
tar -C "$SCRATCH/check" -xf "$SCRATCH/check.tar"
for f in root.key release.key; do
	cmp -s "$KEYS/$f" "$SCRATCH/check/svx-keys/$f" || { echo "verification failed for $f; delete $FILE"; exit 1; }
done
"$REL" check-keys --root trust/production/1.root.json --roots trust/production --key "$SCRATCH/check/svx-keys/root.key" --key "$SCRATCH/check/svx-keys/release.key" >/dev/null
chmod 600 "$FILE"
echo
echo "backup verified: $FILE ($(wc -c < "$FILE") bytes)"
echo "now keep copies in at least two other places: a cloud drive, a USB stick, a printout"
echo "the passphrase belongs only in your password manager; without it the file is useless"
