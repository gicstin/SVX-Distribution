#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p out
exec 9>out/.release.lock
flock -n 9 || { echo "another prepare, sign or publish run is still busy"; exit 1; }
KEY="${1:-$HOME/svx-keys/release.key}"
[ -f "$KEY" ] || { echo "usage: tools/sign-index.sh [RELEASE_KEY]  (re-signs the release index after editing releases/catalog.json; key defaults to ~/svx-keys/release.key)"; exit 2; }
cargo update -q --workspace --offline
cargo build -q --locked --release -p svx-release
REL="${CARGO_TARGET_DIR:-$ROOT/target}/release/svx-release"
CHANGELOG="${SVX_CHANGELOG:-$(realpath -m "${SVX_GAME_REPO:-$ROOT/../SVX}")/CHANGELOG.md}"
if [ -f "$CHANGELOG" ]; then "$REL" changelog --catalog releases/catalog.json --file "$CHANGELOG"; fi
"$REL" index-dir --catalog releases/catalog.json --key "$KEY" --roots trust/production --out out/index --prev out/index/index.json
"$REL" verify --root trust/production/1.root.json --roots trust/production --index out/index/index.json
"$REL" publish-plan --release-dir out/index --repo "${SVX_DIST_REPO:-gicstin/SVX-Distribution}" --tag index --kind index --out out/index/publish-index.sh
{ grep -v '/out/index/publish-index.sh$' out/publish-plan.txt 2>/dev/null || true; echo "$ROOT/out/index/publish-index.sh"; } > out/publish-plan.tmp
mv out/publish-plan.tmp out/publish-plan.txt
echo "re-signed; run \"Publish prepared release\" (tools/publish.sh) to upload it"
