#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
export PATH="$HOME/.cargo/bin:$PATH"

PLATFORMS="${SMOKE_PLATFORMS:-linux windows}"
WORK="${SMOKE_WORK:-$ROOT/work/smoke}"
rm -rf "$WORK"
mkdir -p "$WORK"

echo "== build"
cargo build -q -p svx-release -p testkit --bins
cargo build -q -p svx-patcher
REL="$ROOT/target/debug/svx-release"
SERVER="$ROOT/target/debug/svx-testserver"
if [[ " $PLATFORMS " == *" windows "* ]]; then
	cargo build -q -p svx-patcher --target x86_64-pc-windows-gnu
	cargo build -q -p testkit --bin svx-fixture-game --target x86_64-pc-windows-gnu
	command -v wine >/dev/null || { echo "wine not found; set SMOKE_PLATFORMS=linux"; exit 1; }
fi

"$REL" test-keys --out "$WORK/keys" >/dev/null
KEY="$WORK/keys/TEST-ONLY-release.key"
SITE="$WORK/site"
CATALOG="$WORK/catalog.json"
mkdir -p "$SITE"


payload() {
	local dir="$1" plat="$2" version="$3"
	rm -rf "$dir"
	mkdir -p "$dir/Core/textures" "$dir/Content/SVX/demo"
	if [ "$plat" = linux ]; then
		cp "$ROOT/target/debug/svx-fixture-game" "$dir/SVX.x86_64"
		chmod 755 "$dir/SVX.x86_64"
		local lib="libsvx.linux.template_release.x86_64.so" exe="SVX.x86_64"
	else
		cp "$ROOT/target/x86_64-pc-windows-gnu/debug/svx-fixture-game.exe" "$dir/SVX.exe"
		local lib="libsvx.windows.template_release.x86_64.dll" exe="SVX.exe"
	fi
	printf '\nSVX-SMOKE-%s\n' "$version" >> "$dir/$exe"
	head -c 300000 /dev/urandom > "$dir/$lib"
	[ "$version" = 1.0.0 ] && cp "$dir/$lib" "$WORK/lib-$plat" || cp "$WORK/lib-$plat" "$dir/$lib"
	printf 'SVX smoke readme\n' > "$dir/README.txt"
	seq 1 40000 | sed "s/^/core $version line /" > "$dir/Core/presets.json"
	seq 1 90000 > "$dir/Core/textures/stable.dds"
	[ "$version" = 1.1.0 ] && printf 'new in 1.1.0\n' > "$dir/Core/added.txt"
	printf 'not shipped\n' > "$dir/Content/SVX/demo/x.txt"
	printf '{"version":"%s","build":"%s","commit":"smoke","platform":"%s","date":"2026-10-10T00:00:00Z"}\n' "$version" "${version//./}" "$plat" > "$dir/release.json"
}

publish() {
	local latest="$1"
	python3 - "$CATALOG" "$latest" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
for p in c["platforms"].values():
    if any(r["version"] == sys.argv[2] for r in p["releases"]):
        p["latest"] = sys.argv[2]
json.dump(c, open(sys.argv[1], "w"), indent=1)
PY
	"$REL" index-dir --catalog "$CATALOG" --key "$KEY" --roots trust/test --out "$SITE" --prev "$SITE/index.json" >/dev/null
}

echo "== release fixtures"
for plat in $PLATFORMS; do
	pid="$plat-x86_64"
	prev=""
	for v in 1.0.0 1.1.0; do
		payload "$WORK/payload/$plat-$v" "$plat" "$v"
		out="$WORK/rel/$plat-$v"
		args=(release --payload "$WORK/payload/$plat-$v" --platform "$pid" --min-patcher 0.1.0 --key "$KEY" --out "$out"
			--asset-base "" --manifest-url "releases/svx-$v-$plat/manifest-$pid.json")
		[ -n "$prev" ] && args+=(--reuse "$prev")
		"$REL" "${args[@]}" | tail -2
		mkdir -p "$SITE/releases/svx-$v-$plat"
		cp "$out"/assets/* "$SITE/releases/svx-$v-$plat/"
		"$REL" catalog-add --catalog "$CATALOG" --platform "$pid" --entry "$out/release-entry.json" >/dev/null
		prev="$out/assets/manifest-$pid.json=http://127.0.0.1/releases/svx-$v-$plat/manifest-$pid.json"
	done
	"$REL" publish-plan --release-dir "$WORK/rel/$plat-1.1.0" --repo gicstin/SVX-Distribution --tag "svx-1.1.0" --kind game --out "$WORK/publish-$plat.sh" >/dev/null
done
printf '# Changelog\n\n## [Unreleased]\n\n## [1.1.0] - 2026-10-11\n### Added\n- Added file\n\n## [1.0.0] - 2026-10-10\n- First\n' > "$WORK/CHANGELOG.md"
"$REL" changelog --catalog "$CATALOG" --file "$WORK/CHANGELOG.md" --require 1.1.0 >/dev/null
publish 1.0.0
"$REL" verify --root trust/test/1.root.json --index "$SITE/index.json"

"$SERVER" --root "$SITE" --port-file "$WORK/port" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
for _ in $(seq 50); do [ -s "$WORK/port" ] && break; sleep 0.1; done
PORT="$(cat "$WORK/port")"
export SVX_PATCHER_INDEX_URL="http://127.0.0.1:$PORT/index.json"

FAILS=0
check() {
	local report="$1" expr="$2" what="$3"
	if python3 -c "import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if ($expr) else 1)" "$report" 2>/dev/null; then
		echo "  ok   $what"
	else
		echo "  FAIL $what"; cat "$report" 2>/dev/null || true
		FAILS=$((FAILS + 1))
	fi
}

tree_hash() { (cd "$1" && find Content User Cache logs -type f -print0 2>/dev/null | sort -z | xargs -0 sha256sum; find Content User Cache logs -printf '%p %y %m %T@\n' 2>/dev/null | sort) | sha256sum | cut -d' ' -f1; }

scenario() {
	local plat="$1"
	local dir="$WORK/install-$plat"
	local cfg="$WORK/cfg-$plat"
	local run exe dirarg
	export SVX_PATCHER_CONFIG="$cfg"
	if [ "$plat" = linux ]; then
		run=("$ROOT/target/debug/svx-patcher" --cli)
		exe="SVX.x86_64"
		dirarg="$dir"
	else
		export WINEPREFIX="$WORK/wineprefix" WINEDEBUG=-all
		run=(wine "$ROOT/target/x86_64-pc-windows-gnu/debug/svx-patcher.exe" --cli)
		exe="SVX.exe"
		dirarg="Z:${dir//\//\\}"
		export SVX_PATCHER_CONFIG="Z:${cfg//\//\\}"
	fi
	local r="$WORK/report-$plat.json"
	p() { rm -f "$r"; "${run[@]}" "$@" --report "$(rep)" > "$WORK/last-$plat.log" 2>&1 || true; }
	rep() { if [ "$plat" = linux ]; then echo "$r"; else echo "Z:${r//\//\\}"; fi; }

	echo "== $plat"
	mkdir -p "$dir/Content/SVX/maren/1" "$dir/User" "$dir/logs" "$dir/Cache"
	head -c 200000 /dev/urandom > "$dir/Content/SVX/maren/1/figure.svxfig"
	printf '{"vr":true}\n' > "$dir/User/settings.json"
	printf 'old log\n' > "$dir/logs/latest.log"
	printf 'cache\n' > "$dir/Cache/x.bin"
	local before; before="$(tree_hash "$dir")"
	publish 1.0.0
	export SVX_FIXTURE_MARKER="$WORK/marker-$plat"
	[ "$plat" = windows ] && export SVX_FIXTURE_MARKER="Z:${WORK//\//\\}\\marker-$plat"
	rm -f "$WORK/marker-$plat"

	p status --dir "$dirarg"
	check "$r" 'r["status"]["state"]=="absent"' "absent before install"
	p check --dir "$dirarg"
	check "$r" 'r["latest"]=="1.0.0" and r["offered"]==["1.1.0","1.0.0"] and r["test_build"]' "check sees signed index"
	check "$r" '[e["version"] for e in r["changelog"]]==["1.1.0","1.0.0"] and r["changelog"][0]["sections"][0]["items"]==["Added file"]' "check sees signed changelog"
	p install --dir "$dirarg" --play
	check "$r" 'r["ok"] and r["done"]["applied"]=="1.0.0" and r["done"]["launched"]' "fresh install and play"
	[ -s "$WORK/marker-$plat" ] && echo "  ok   game launched" || { echo "  FAIL game launched"; FAILS=$((FAILS + 1)); }

	publish 1.1.0
	p check --dir "$dirarg"
	check "$r" 'r["latest"]=="1.1.0"' "update visible"
	p install --dir "$dirarg"
	check "$r" 'r["ok"] and r["done"]["applied"]=="1.1.0"' "update to 1.1.0"
	cmp -s "$dir/$exe" "$WORK/payload/$plat-1.1.0/$exe" && echo "  ok   exe matches 1.1.0" || { echo "  FAIL exe"; FAILS=$((FAILS + 1)); }
	[ -f "$dir/Core/added.txt" ] && echo "  ok   added file present" || { echo "  FAIL added file"; FAILS=$((FAILS + 1)); }

	SVX_PATCHER_ABORT_AFTER_OPS=2 p install --dir "$dirarg" --version 1.0.0
	p status --dir "$dirarg"
	check "$r" 'r["status"]["version"]=="1.0.0" and "interrupted" in (r["note"] or "")' "crash during activation recovered on next start"
	cmp -s "$dir/$exe" "$WORK/payload/$plat-1.0.0/$exe" && [ ! -e "$dir/Core/added.txt" ] && echo "  ok   files match 1.0.0" || { echo "  FAIL files after recovery"; FAILS=$((FAILS + 1)); }

	p rollback --dir "$dirarg"
	check "$r" 'r["ok"] and r["status"]["version"]=="1.1.0"' "rollback to previous version"

	rm -f "$dir/$exe"
	printf 'garbage' > "$dir/Core/presets.json"
	p status --dir "$dirarg"
	check "$r" 'r["status"]["health"] is not None' "damage detected"
	p play --dir "$dirarg"
	check "$r" 'not r["ok"] and r["error_kind"]=="Damaged"' "play refuses damaged install"
	p repair --dir "$dirarg"
	check "$r" 'r["ok"] and r["status"]["health"] is None and r["status"]["version"]=="1.1.0"' "repair"

	if [ "$plat" = linux ]; then
		SVX_FIXTURE_SLEEP=30 "$dir/$exe" &
	else
		SVX_FIXTURE_SLEEP=30 wine "$dir/$exe" &
	fi
	local game=$!
	mkdir -p "$WORK/other-$plat"
	cp "$dir/$exe" "$WORK/other-$plat/$exe"
	if [ "$plat" = linux ]; then
		SVX_FIXTURE_SLEEP=30 "$dir/$exe" &
		SVX_FIXTURE_SLEEP=30 "$WORK/other-$plat/$exe" &
	else
		SVX_FIXTURE_SLEEP=30 wine "$dir/$exe" &
		SVX_FIXTURE_SLEEP=30 wine "$WORK/other-$plat/$exe" &
	fi
	local other=$!
	sleep 3
	p install --dir "$dirarg" --version 1.0.0 --no-wait
	check "$r" 'not r["ok"] and r.get("waited_for_game") and r["status"]["version"]=="1.1.0"' "running game blocks replacement"
	p running --dir "$dirarg"
	check "$r" 'len(r["running"])==2' "both running copies found"
	p close-game --dir "$dirarg"
	check "$r" 'r["ok"] and r["closed"]==2 and r["remaining"]==0' "force close ends both copies"
	p running --dir "$dirarg"
	check "$r" 'r["running"]==[]' "nothing left running in the install"
	kill -0 "$other" 2>/dev/null && echo "  ok   copy outside the install kept running" || { echo "  FAIL other copy was closed"; FAILS=$((FAILS + 1)); }
	kill "$other" "$game" 2>/dev/null || true
	[ "$plat" = windows ] && { wineserver -k 2>/dev/null || true; sleep 1; }
	wait "$game" "$other" 2>/dev/null || true
	p install --dir "$dirarg" --version 1.0.0
	check "$r" 'r["ok"] and r["done"]["applied"]=="1.0.0"' "downgrade after game closed"

	local after; after="$(tree_hash "$dir")"
	[ "$before" = "$after" ] && echo "  ok   Content/User/Cache/logs untouched" || { echo "  FAIL protected folders changed"; FAILS=$((FAILS + 1)); }
}

for plat in $PLATFORMS; do
	scenario "$plat"
done
[ "$FAILS" = 0 ] && echo "smoke passed" || { echo "smoke: $FAILS failure(s)"; exit 1; }
