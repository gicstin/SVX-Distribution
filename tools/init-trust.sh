#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
KEYS="${1:-$HOME/svx-keys}"
YEARS="${2:-5}"
OUT=trust/production/1.root.json
[ -f "$OUT" ] && { echo "$OUT already exists; rotate with svx-release root --version 2 instead"; exit 1; }
case "$(realpath -m "$KEYS")/" in "$(realpath .)"/*) echo "keep keys outside the repository: $KEYS"; exit 1 ;; esac
for name in root release; do
	[ -f "$KEYS/$name.key" ] || tools/cargo.sh run -q -p svx-release -- keygen --out "$KEYS" --name "$name"
done
EXPIRES="$(date -u -d "+$YEARS years" +%Y-%m-%dT00:00:00Z)"
mkdir -p trust/production
tools/cargo.sh run -q -p svx-release -- root --version 1 --expires "$EXPIRES" --root-pub "$KEYS/root.pub" --release-pub "$KEYS/release.pub" --sign "$KEYS/root.key" --out "$OUT"
tools/cargo.sh run -q -p svx-release -- verify --root "$OUT"
echo "commit $OUT; back up $KEYS/root.key and $KEYS/release.key somewhere safe, then move root.key offline"
