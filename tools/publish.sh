#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
REPO="${SVX_DIST_REPO:-gicstin/SVX-Distribution}"
mkdir -p out
exec 9>out/.release.lock
flock -n 9 || { echo "another prepare, sign or publish run is still busy"; exit 1; }
PLAN="$ROOT/out/publish-plan.txt"
[ -s "$PLAN" ] || { echo "nothing prepared; run Prepare ... release or Re-sign index first"; exit 1; }
command -v gh >/dev/null || { echo "the GitHub CLI (gh) is required"; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "log in first: gh auth login"; exit 1; }
VIS="$(gh api "repos/$REPO" --jq .visibility 2>/dev/null || echo unknown)"
echo "about to upload to github.com/$REPO ($VIS):"
while IFS= read -r s; do
	[ -x "$s" ] || { echo "missing $s; prepare again"; exit 1; }
	echo "  $s ($(grep -c '^gh release upload' "$s") files)"
done < "$PLAN"
[ "$VIS" = public ] || echo "WARN the repository is $VIS; players cannot download until it is public"
read -r -p "type yes to publish: " answer
[ "$answer" = yes ] || { echo "cancelled"; exit 1; }
while IFS= read -r s; do
	echo "== $s"
	"$s"
	touch "$(dirname "$s")/.published"
done < "$PLAN"
rm -f "$PLAN"
echo "published. commit releases/catalog.json if it changed"
