#!/usr/bin/env bash
# check-slides.sh [DECK.md …] — render a Marp presentation and check every
# slide fits its 16:9 frame (#77). Default deck: docs/presentation.md.
#
#   sc-build tools/check-slides.sh
#   sc-build 'tools/check-slides.sh docs/presentation.md'
#
# The build box has no browser installed for the build user, and under
# "nothing kept" (owner, 2026-09-28) none is (#77). So, like deploy/test.sh,
# this installs Playwright's headless chromium into the job's own scratch
# directory (deleted afterwards) and points Marp at it with CHROME_PATH — then
# marp-cli can render `--pdf` / `--images png`, which fail with "No suitable
# browser found" without one. Liberation fonts (metric-compatible with Arial)
# are already on the Fedora build box, so the measured fit is accurate; if a
# future box lacks them the fit is approximate and that is said.
#
# For each deck it renders the HTML (template `bare`, every slide visible) and
# the per-slide PNGs, then tools/slide-fit.mjs measures each slide's content
# height against the 720 px frame and fails if any overflows (sc#15: 9 of 12
# slides ran past the frame and nobody had seen it, because nothing could
# render it). Run it when a deck changes. Copied from stormcentral's deploy/
# (#77), with the paths moved to tools/.
set -euo pipefail
cd "$(dirname "$0")/.."

decks=("$@")
[ ${#decks[@]} -gt 0 ] || decks=("docs/presentation.md")
for md in "${decks[@]}"; do
  [ -f "$md" ] || { echo "check-slides: no deck $md" >&2; exit 2; }
done

PLAYWRIGHT_VERSION=${PLAYWRIGHT_VERSION:-1.63.0}
MARP_VERSION=${MARP_VERSION:-4}
# Scratch on the job's own drive (tmp/ is in .gitignore); never ~/tmp or /tmp.
mkdir -p tmp
W=$(mktemp -d -p "${TMPDIR:-$PWD/tmp}" sc-slides.XXXXXX)
# Marp (puppeteer) can leave a headless chromium holding the pipe, which hangs
# the job (#37); kill any stray before the scratch goes.
trap 'pkill -u "$(id -u)" -f chrome-headless-shell 2>/dev/null || true; rm -rf "$W"' EXIT

echo "==> headless chromium (Playwright $PLAYWRIGHT_VERSION, scratch)"
export PW_DIR="$W/pw" PLAYWRIGHT_BROWSERS_PATH="$W/pw/browsers"
npm install --prefix "$PW_DIR" --no-audit --no-fund --loglevel=error "playwright@$PLAYWRIGHT_VERSION" >/dev/null
"$PW_DIR/node_modules/.bin/playwright" install chromium-headless-shell >/dev/null
# Playwright 1.63 names the headless-shell binary `chrome-headless-shell`.
CHROME_PATH=$(find "$PLAYWRIGHT_BROWSERS_PATH" -type f -name chrome-headless-shell | head -1)
[ -n "$CHROME_PATH" ] || { echo "check-slides: chromium-headless-shell did not install (no binary under $PLAYWRIGHT_BROWSERS_PATH)" >&2; exit 1; }
export CHROME_PATH
echo "    CHROME_PATH=$CHROME_PATH"

# Check the font files on disk, not fc-list (whose cache may be cold on a
# fresh VM and then report nothing, though chromium builds its own cache).
if find /usr/share/fonts -iname 'LiberationSans*' 2>/dev/null | grep -q .; then
  echo "    fonts: Liberation Sans present (Arial metrics) — fit is accurate"
else
  echo "    fonts: no Liberation Sans — Arial falls back, the measured fit is approximate" >&2
fi

fail=0
for md in "${decks[@]}"; do
  name=$(basename "$md" .md)
  in="$W/$name-in"; out="$W/$name-out"
  mkdir -p "$in" "$out"
  cp "$md" "$in/$name.md"
  echo "==> $md: render + fit check"
  # HTML (bare: every slide a visible <section>, for measuring). No browser
  # needed for HTML, so this works even if the browser is missing.
  timeout 300 npx -y "@marp-team/marp-cli@$MARP_VERSION" --html --template bare \
    --allow-local-files -I "$in" -o "$out" </dev/null
  # PNGs — this is the step that needs the browser (what #77 is about). It
  # proves marp `--images png` renders here now, and leaves the images as
  # artifacts under the scratch.
  timeout 300 npx -y "@marp-team/marp-cli@$MARP_VERSION" --images png --image-scale 1 \
    --allow-local-files -I "$in" -o "$out" </dev/null
  pngs=$(find "$out" -name '*.png' | wc -l)
  echo "    rendered $pngs PNG(s)"
  [ "$pngs" -gt 0 ] || { echo "check-slides: $md rendered no PNGs — the browser did not render" >&2; fail=1; continue; }
  node tools/slide-fit.mjs "$out/$name.html" || fail=1
done

[ "$fail" -eq 0 ] && echo "==> check-slides: ok" || echo "==> check-slides: a deck has an overflowing slide" >&2
exit "$fail"
