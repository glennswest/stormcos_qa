// tools/slide-fit.mjs <deck.html> [frameW frameH] — does every slide fit? (#77)
//
// Loads a Marp deck rendered with `--template bare` (every slide a visible
// <section>) in Playwright's headless chromium, pins each slide to the real
// 16:9 frame, and measures its content height. A slide whose content is taller
// than the frame overflows — its bottom is cut off on screen and in the PDF,
// which nobody could see before because the build box had no browser. Prints a
// line per slide and exits 1 if any overflows.
//
// Playwright is required from $PW_DIR (installed into the job's scratch by the
// caller, as deploy/test.sh does), so this needs no browser on the build box.
import { createRequire } from 'node:module';
import path from 'node:path';

const require = createRequire(import.meta.url);
const html = process.argv[2];
const W = Number(process.argv[3] || process.env.SLIDE_W || 1280);
const H = Number(process.argv[4] || process.env.SLIDE_H || 720);
if (!html) {
  console.error('usage: slide-fit.mjs <deck.html> [W H]');
  process.exit(2);
}
const pwDir = process.env.PW_DIR;
if (!pwDir) {
  console.error('slide-fit: $PW_DIR is not set (the per-job Playwright directory)');
  process.exit(2);
}
const { chromium } = require(path.join(pwDir, 'node_modules', 'playwright'));

const browser = await chromium.launch({ args: ['--no-sandbox'] });
try {
  const page = await browser.newPage({ viewport: { width: W, height: H }, deviceScaleFactor: 1 });
  await page.goto('file://' + path.resolve(html), { waitUntil: 'load' });
  // Metric-compatible fonts must be in before measuring (Liberation = Arial).
  await page.evaluate(() => (document.fonts ? document.fonts.ready : null));
  await page.waitForTimeout(300);
  const slides = await page.evaluate(({ W, H }) => {
    // Pin every slide to the real frame: its own width, a fixed height, and
    // overflow hidden — so scrollHeight is the true content height and is
    // greater than the frame exactly when the slide overflows.
    const st = document.createElement('style');
    st.textContent =
      `section{position:static!important;transform:none!important;margin:0!important;` +
      `width:${W}px!important;height:${H}px!important;overflow:hidden!important;box-sizing:border-box!important;}`;
    document.head.appendChild(st);
    return [...document.querySelectorAll('section')].map((s, i) => {
      const h = s.querySelector('h1,h2,h3,h4');
      return { n: i + 1, content: s.scrollHeight, title: (h ? h.textContent : '').trim().slice(0, 44) };
    });
  }, { W, H });

  if (slides.length === 0) {
    console.error(`slide-fit: no <section> slides found in ${html} (was it rendered with --template bare?)`);
    process.exit(2);
  }
  const tol = 2; // a pixel or two of rounding is not an overflow
  let over = 0;
  console.log(`slide fit (frame ${W}×${H}, ${slides.length} slides):`);
  for (const s of slides) {
    const bad = s.content > H + tol;
    if (bad) over++;
    const mark = bad ? `OVERFLOW (+${s.content - H}px)` : 'ok';
    console.log(`  slide ${String(s.n).padStart(2)}  ${s.content}px / ${H}px  ${mark}${s.title ? '  ' + s.title : ''}`);
  }
  if (over) {
    console.error(`SLIDE FIT: ${over} of ${slides.length} slide(s) overflow the ${W}×${H} frame — trim them or split the slide`);
    process.exitCode = 1;
  } else {
    console.log(`SLIDE FIT: all ${slides.length} slides fit the ${W}×${H} frame`);
  }
} finally {
  await browser.close();
}
