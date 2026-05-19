#!/usr/bin/env bash
# port-page.sh — porting workflow driver for a single page.
#
# 1. Render the typst entrypoint with twyla-render-page.
# 2. Diff the full rendered HTML against the zola-built page.
#
# Today: hardcoded for guis-2. Generalize once we port a second page and
# can see what varies.

set -euo pipefail

TWYLA_ROOT="${TWYLA_ROOT:-$HOME/Src/twyla}"
SITE_ROOT="${SITE_ROOT:-$HOME/Src/site}"

ENTRYPOINT="$SITE_ROOT/content/guis-2.typ"
ZOLA_HTML="$SITE_ROOT/public/guis-2/index.html"

WORK="$(mktemp -d -t twyla-port-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

cd "$TWYLA_ROOT"
cargo build --quiet --bin twyla-render-page --bin twyla-diff

TYPST_HTML="$WORK/typst.html"

echo ">>> rendering $ENTRYPOINT"
./target/debug/twyla-render-page --root "$SITE_ROOT" "$ENTRYPOINT" > "$TYPST_HTML"

echo ">>> diff"
./target/debug/twyla-diff \
  --textonly-pre \
  --ignore-attr td:style \
  --ignore-attr th:style \
  "$ZOLA_HTML" "$TYPST_HTML"

echo
echo "intermediate files preserved at: $WORK"
trap - EXIT
