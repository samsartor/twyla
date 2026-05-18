#!/usr/bin/env bash
# port-page.sh — porting workflow driver for a single page.
#
# 1. Render the typst entrypoint with twyla-render-page.
# 2. Extract the body subtree from both the typst-rendered HTML and the
#    pre-built zola HTML.
# 3. Diff them with twyla-diff under the porting-relevant relaxations.
#
# Today: hardcoded for guis-2. Generalize once we port a second page and
# can see what varies.

set -euo pipefail

TWYLA_ROOT="${TWYLA_ROOT:-$HOME/Src/twyla}"
SITE_ROOT="${SITE_ROOT:-$HOME/Src/site}"

ENTRYPOINT="$SITE_ROOT/content/guis-2.typ"
ZOLA_HTML="$SITE_ROOT/public/guis-2/index.html"
ZOLA_BODY_SELECTOR="class:post__body"
TYPST_BODY_SELECTOR="tag:body"

WORK="$(mktemp -d -t twyla-port-XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

cd "$TWYLA_ROOT"
cargo build --quiet --bin twyla-render-page --bin twyla-extract --bin twyla-diff

TYPST_RAW="$WORK/typst-raw.html"
TYPST_BODY="$WORK/typst-body.html"
ZOLA_BODY="$WORK/zola-body.html"

echo ">>> rendering $ENTRYPOINT"
./target/debug/twyla-render-page --root "$SITE_ROOT" "$ENTRYPOINT" > "$TYPST_RAW"

echo ">>> extracting typst body ($TYPST_BODY_SELECTOR)"
./target/debug/twyla-extract "$TYPST_BODY_SELECTOR" "$TYPST_RAW" > "$TYPST_BODY"

echo ">>> extracting zola body ($ZOLA_BODY_SELECTOR)"
./target/debug/twyla-extract "$ZOLA_BODY_SELECTOR" "$ZOLA_HTML" > "$ZOLA_BODY"

echo ">>> diff"
./target/debug/twyla-diff \
  --textonly-pre \
  --ignore-attr td:style \
  --ignore-attr th:style \
  "$ZOLA_BODY" "$TYPST_BODY"

echo
echo "intermediate files preserved at: $WORK"
trap - EXIT
