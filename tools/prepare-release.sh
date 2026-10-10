#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
export PATH="$HOME/.cargo/bin:$PATH"

usage() {
	echo "usage: tools/prepare-release.sh [--key RELEASE_KEY ...] [--game] [--linux PAYLOAD_DIR] [--windows PAYLOAD_DIR]"
	echo "         [--reuse PREVIOUS_VERSION|auto] [--min-patcher V] [--notes TEXT] [--patcher out/patcher/V|auto] [--latest]"
	echo "  --key defaults to ~/svx-keys/release.key"
	echo "  --game takes both payloads from \$SVX_GAME_REPO/dist (default ../SVX), made by tools/package.sh <platform> release --payload"
	echo "  --patcher auto takes the newest release build in out/patcher; --reuse auto reuses the newest earlier release in out/releases"
	echo "  writes out/releases/<version>/ (assets + publish-release.sh), updates releases/catalog.json and re-signs out/index/"
	echo "  nothing is uploaded; review and run the generated scripts yourself"
}

REPO="${SVX_DIST_REPO:-gicstin/SVX-Distribution}"
mkdir -p out
exec 9>out/.release.lock
flock -n 9 || { echo "another prepare, sign or publish run is still busy"; exit 1; }
KEYS=()
declare -A PAYLOADS=()
REUSE=""
MIN="0.1.0"
NOTES=""
PATCHER=""
LATEST=0
GAME=0
while [ $# -gt 0 ]; do
	case "$1" in
		--key) KEYS+=(--key "$2"); shift 2 ;;
		--linux) PAYLOADS[linux]="$2"; shift 2 ;;
		--windows) PAYLOADS[windows]="$2"; shift 2 ;;
		--reuse) REUSE="$2"; shift 2 ;;
		--min-patcher) MIN="$2"; shift 2 ;;
		--notes) NOTES="$2"; shift 2 ;;
		--patcher) PATCHER="$2"; shift 2 ;;
		--game) GAME=1; shift ;;
		--latest) LATEST=1; shift ;;
		-h|--help) usage; exit 0 ;;
		*) usage; exit 2 ;;
	esac
done
if [ ${#KEYS[@]} -eq 0 ]; then
	[ -f "$HOME/svx-keys/release.key" ] || { echo "no --key given and ~/svx-keys/release.key is missing; restore it with tools/restore-keys.sh"; exit 1; }
	KEYS=(--key "$HOME/svx-keys/release.key")
fi
if [ "$GAME" = 1 ]; then
	GAME_REPO="$(realpath -m "${SVX_GAME_REPO:-$ROOT/../SVX}")"
	for plat in linux windows; do
		dir="$GAME_REPO/dist/SVX-$plat-release-payload"
		if [ -f "$dir/release.json" ]; then
			PAYLOADS[$plat]="$dir"
		else
			echo "WARN no $plat payload at $dir (run Package - ${plat^} Release Payload in the SVX project)"
		fi
	done
	[ ${#PAYLOADS[@]} -gt 0 ] || { echo "no game payloads found under $GAME_REPO/dist"; exit 1; }
fi
if [ "$PATCHER" = auto ]; then
	PATCHER="$(find "$ROOT/out/patcher" -mindepth 1 -maxdepth 1 -type d ! -name '*-test' 2>/dev/null | sort -V | tail -1 || true)"
	[ -n "$PATCHER" ] || { echo "no release patcher build in out/patcher; run Build release patchers first"; exit 1; }
fi
[ -f trust/production/1.root.json ] || { echo "missing trust/production/1.root.json"; exit 1; }

cargo build -q --locked --release -p svx-release
REL="${CARGO_TARGET_DIR:-$ROOT/target}/release/svx-release"
CATALOG="$ROOT/releases/catalog.json"

VERSION=""
for plat in "${!PAYLOADS[@]}"; do
	v="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "${PAYLOADS[$plat]}/release.json")"
	[ -z "$VERSION" ] || [ "$VERSION" = "$v" ] || { echo "payload versions differ: $VERSION vs $v"; exit 1; }
	VERSION="$v"
done

if [ "$REUSE" = auto ]; then
	REUSE="$(find "$ROOT/out/releases" -mindepth 1 -maxdepth 1 -type d ! -name "$VERSION" -printf '%f\n' 2>/dev/null | sort -V | tail -1 || true)"
	if [ -n "$REUSE" ]; then echo "reusing unchanged files from $REUSE"; fi
fi

PLAN="$ROOT/out/publish-plan.txt"
rm -f "$PLAN"
mkdir -p "$ROOT/out"

if [ -n "$VERSION" ]; then
	TAG="svx-$VERSION"
	OUT="$ROOT/out/releases/$VERSION"
	rm -rf "$OUT"
	mkdir -p "$OUT/assets"
	for plat in "${!PAYLOADS[@]}"; do
		pid="$plat-x86_64"
		args=(release --payload "${PAYLOADS[$plat]}" --platform "$pid" --min-patcher "$MIN" "${KEYS[@]}" --github "$REPO" --tag "$TAG" --out "$OUT/$plat")
		[ -n "$NOTES" ] && args+=(--notes "$NOTES")
		if [ -n "$REUSE" ]; then
			prev="$ROOT/out/releases/$REUSE/$plat/assets/manifest-$pid.json"
			[ -f "$prev" ] || { echo "missing $prev for --reuse"; exit 1; }
			args+=(--reuse "$prev=https://github.com/$REPO/releases/download/svx-$REUSE/manifest-$pid.json")
		fi
		echo "== packing $plat ${VERSION} (compressing ~$(du -sh "${PAYLOADS[$plat]}" | cut -f1), takes a while)"
		"$REL" "${args[@]}"
		cp "$OUT/$plat/assets/"* "$OUT/assets/"
		add=(catalog-add --catalog "$CATALOG" --platform "$pid" --entry "$OUT/$plat/release-entry.json")
		[ "$LATEST" = 1 ] && add+=(--latest)
		"$REL" "${add[@]}"
	done
	"$REL" publish-plan --release-dir "$OUT" --repo "$REPO" --tag "$TAG" --kind game --out "$OUT/publish-release.sh"
fi

if [ -n "$PATCHER" ]; then
	PV="$(basename "$PATCHER")"
	case "$PV" in *-test) echo "refusing a test patcher build: $PATCHER"; exit 1 ;; esac
	PTAG="patcher-$PV"
	POUT="$ROOT/out/patcher-release/$PV"
	rm -rf "$POUT"
	mkdir -p "$POUT/assets"
	cp "$PATCHER/svx-patcher" "$PATCHER/SVXPatcher.exe" "$POUT/assets/"
	echo "$POUT/publish-patcher.sh" >> "$PLAN"
	"$REL" patcher --catalog "$CATALOG" --version "$PV" --base-url "https://github.com/$REPO/releases/download/$PTAG/" --linux "$POUT/assets/svx-patcher" --windows "$POUT/assets/SVXPatcher.exe"
	"$REL" publish-plan --release-dir "$POUT" --repo "$REPO" --tag "$PTAG" --kind patcher --out "$POUT/publish-patcher.sh"
fi

if [ -z "$PATCHER" ]; then
	PV="$(python3 -c 'import json,sys; p=json.load(open(sys.argv[1])).get("patcher"); print(p["version"] if p else "")' "$CATALOG")"
	PENDING="$ROOT/out/patcher-release/$PV/publish-patcher.sh"
	if [ -n "$PV" ] && [ -x "$PENDING" ] && [ ! -e "$ROOT/out/patcher-release/$PV/.published" ]; then
		echo "patcher $PV is prepared but not published yet; including it"
		{ echo "$PENDING"; cat "$PLAN" 2>/dev/null || true; } > "$PLAN.tmp"
		mv "$PLAN.tmp" "$PLAN"
	fi
fi

"$REL" index-dir --catalog "$CATALOG" "${KEYS[@]}" --roots trust/production --out "$ROOT/out/index" --prev "$ROOT/out/index/index.json"
"$REL" verify --root trust/production/1.root.json --roots trust/production --index "$ROOT/out/index/index.json"
"$REL" publish-plan --release-dir "$ROOT/out/index" --repo "$REPO" --tag index --kind index --out "$ROOT/out/index/publish-index.sh"
if [ -n "$VERSION" ]; then echo "$OUT/publish-release.sh" >> "$PLAN"; fi
echo "$ROOT/out/index/publish-index.sh" >> "$PLAN"
echo
echo "prepared; nothing was uploaded. Run \"Publish prepared release\" (tools/publish.sh) to upload:"
sed 's/^/  /' "$PLAN"
echo "then commit releases/catalog.json"
