// Drive the real SPA in a real browser, with no display.
//
// `tsc` cannot see any of what this checks: whether the justified layout
// produces cells, whether those cells fetch thumbnails that actually arrive,
// whether the viewer opens on a click, whether the event stream connects. Every
// one of those is a runtime question, and the grid is the part of this
// application most worth asking it about.
//
// Starts the binary the same way a person does — `lightview <dir>` — takes the
// launch URL off stdout, and lets the page redeem the token itself, so the
// boot path under test is the real one.

import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

// The pre-installed browser, launched by path.
//
// Playwright resolves a browser by *revision*, and the revision this project's
// version wants is not the one the image ships — so the default launch fails
// with "run npx playwright install", which is exactly what must not happen in
// a sandbox with no network. Naming the executable sidesteps the revision
// lookup entirely, and a Chromium two revisions apart renders this page the
// same way.
const CHROMIUM = process.env.LV_CHROMIUM ?? "/opt/pw-browsers/chromium";

const repo = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const BIN = process.env.BIN ?? join(repo, "src-rust", "target", "debug", "lightview");

let passed = 0;
let failed = 0;
const ok = (m) => { console.log(`  ok   ${m}`); passed++; };
const bad = (m) => { console.log(`  FAIL ${m}`); failed++; };
const check = (m, cond) => (cond ? ok(m) : bad(m));

const work = mkdtempSync(join(tmpdir(), "lv-grid-"));
const gallery = join(work, "gallery");
const state = join(work, "state");

// A gallery with enough cells to fill more than one row, and a portrait among
// them so the justified layout has a reason to compute anything.
const { execFileSync } = await import("node:child_process");
execFileSync("mkdir", ["-p", join(gallery, "2026")]);
for (let i = 0; i < 12; i++) {
  const size = i % 3 === 0 ? "480x640" : "800x600";
  execFileSync("ffmpeg", [
    "-y", "-v", "error", "-f", "lavfi",
    "-i", `testsrc=size=${size}:duration=1`,
    "-frames:v", "1", join(gallery, "2026", `p${i}.png`),
  ]);
}
ok("gallery built (12 files)");

// Install the bundled example tagger into this run's state directory, so the
// plugin path is exercised from the UI as well as from `lightview tag`.
// `--data-dir <root>` maps the three XDG roots to `<root>/{cache,data,config}`.
const installRoot = join(state, "data", "plugins");
execFileSync("mkdir", ["-p", installRoot]);
execFileSync("cp", ["-r", join(repo, "plugins", "example-auto-tagger"), installRoot]);
ok("example tagger installed");

// ---------------------------------------------------------------------------

const server = spawn(BIN, [gallery, "--data-dir", state], { stdio: ["ignore", "pipe", "pipe"] });
let stdout = "";
let stderr = "";
server.stdout.on("data", (d) => (stdout += d));
server.stderr.on("data", (d) => (stderr += d));

const launchUrl = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error(`no URL on stdout:\n${stdout}\n${stderr}`)), 30_000);
  const poll = setInterval(() => {
    const line = stdout.split("\n").find((l) => l.startsWith("http://"));
    if (line) {
      clearInterval(poll);
      clearTimeout(timer);
      resolve(line.trim());
    }
  }, 100);
});
ok(`server printed its launch URL`);

const browser = await chromium.launch({
  executablePath: CHROMIUM,
  args: ["--no-sandbox", "--headless=new"],
});
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });

// Anything the page logs as an error is a failure: a grid that renders while
// throwing is a grid that will stop rendering on the next interaction.
const consoleErrors = [];
page.on("console", (m) => m.type() === "error" && consoleErrors.push(m.text()));
page.on("pageerror", (e) => consoleErrors.push(String(e)));

// Every request the page makes, so a 4xx on a thumbnail cannot hide behind a
// cell that simply stayed blank.
const failedRequests = [];
page.on("response", (r) => {
  if (r.status() >= 400) failedRequests.push(`${r.status()} ${new URL(r.url()).pathname}`);
});

try {
  await page.goto(launchUrl, { waitUntil: "domcontentloaded" });

  // The token is single-use and the app clears it before the first render.
  await page.waitForFunction(() => !window.location.search.includes("t="), { timeout: 15_000 });
  ok("the launch token was redeemed and cleared from the address bar");

  // Cells: the justified layout has run and placed something.
  await page.waitForSelector("img[src*='/thumb/']", { timeout: 30_000 });
  const cells = await page.locator("img[src*='/thumb/']").count();
  check(`the grid rendered ${cells} cells`, cells >= 6);

  // Thumbnails that actually arrived. Waited for rather than sampled: the
  // first `<img>` appears the moment a cell is placed, and a cold gallery
  // generates every tier on demand — measuring here without waiting reads zero
  // and blames the app for the harness being early.
  const decodedCells = () =>
    page.evaluate(() =>
      Array.from(document.querySelectorAll("img[src*='/thumb/']"))
        .filter((i) => i.naturalWidth > 0).length,
    );
  await page
    .waitForFunction(
      () =>
        Array.from(document.querySelectorAll("img[src*='/thumb/']"))
          .filter((i) => i.naturalWidth > 0).length >= 6,
      { timeout: 60_000 },
    )
    .catch(() => {});
  const decoded = await decodedCells();
  check(`${decoded} thumbnails decoded`, decoded >= 6);

  // The layout is justified, not square: rows contain cells of differing
  // widths, because the fixtures are not all the same aspect. Measured on
  // cells that have decoded — an undecoded one is 0 wide and would make any
  // set of widths look varied.
  const widths = await page.evaluate(() =>
    Array.from(document.querySelectorAll("img[src*='/thumb/']"))
      .filter((i) => i.naturalWidth > 0)
      .map((i) => Math.round(i.getBoundingClientRect().width)),
  );
  check("cells have varying widths (justified, not square)", new Set(widths).size > 1);

  // The event stream connected.
  const sse = await page.evaluate(async () => {
    const r = await fetch("/api/events", { method: "HEAD" });
    return r.status;
  });
  check(`the event stream answers (${sse})`, sse < 400);

  // The viewer opens on a click and shows the full image.
  await page.locator("img[src*='/thumb/']").first().click();
  await page.waitForSelector("img[src*='/media/']", { timeout: 20_000 });
  ok("clicking a cell opens the viewer on the full image");

  await page.keyboard.press("Escape");
  await page.waitForTimeout(600);
  check("Escape closes the viewer", (await page.locator("img[src*='/media/']").count()) === 0);

  // Scrolling drives the layout's range recalculation.
  await page.mouse.wheel(0, 4000);
  await page.waitForTimeout(800);
  check("the grid survives a scroll", (await page.locator("img[src*='/thumb/']").count()) > 0);

  // Settings: the trimmed panel opens and its sections are there.
  await page.keyboard.press("i");
  await page.waitForSelector("text=Settings", { timeout: 5000 });
  for (const section of ["Display", "Thumbnails", "Default filter", "Connection"]) {
    check(`settings has a ${section} section`, (await page.locator(`text=${section}`).count()) > 0);
  }
  await page.keyboard.press("Escape");

  // A plugin run, started from the UI and watched to completion.
  //
  // The acceptance criterion for the executor from this side: the panel lists
  // what is installed, Run starts an in-process job, and the run reports itself
  // finished through the same event stream everything else uses.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(300);
  await page.locator("button[title='Actions']").click();
  await page.locator("text=Auto-tagging").click();
  await page.waitForSelector("text=Example Auto-Tagger", { timeout: 10_000 });
  ok("the auto-tag panel lists the installed plugin");

  await page.locator("button", { hasText: /^Run$/ }).first().click();
  // The toast appears while the run is live and goes when it finishes. Either
  // edge is proof it ran; waiting for the *tags* is proof it worked.
  await page.waitForFunction(
    async () => {
      const r = await fetch("/api/invoke", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          command: "get_items",
          args: { filter: "has::plugin.example" },
        }),
      });
      if (!r.ok) return false;
      return (await r.json()).items.length >= 12;
    },
    { timeout: 60_000 },
  );
  ok("the run tagged every file, and the index sees the new namespace");

  // Row geometry, measured off the real DOM.
  //
  // The bug was a ragged right edge at every group boundary. The fix stretches
  // a group's last row to fill the width when that costs little height, and
  // leaves it short when it would not — so the thing to catch is the *other*
  // failure, the one an earlier draft of this fix would have shipped: a short
  // row that is also much taller than the rows above it, which is worse than
  // the gap it was trying to close.
  const rows = await page.evaluate(() => {
    // The layout positions one absolutely-placed div per cell inside a
    // `position: relative` track. Measure those, not the images inside them:
    // a thumbnail is letterboxed within its cell and its own box says nothing
    // about where the row ends.
    const imgs = [...document.querySelectorAll("img[src*='/thumb/']")];
    const placed = imgs
      .map((i) => i.closest("div[style*='position: absolute']"))
      .filter(Boolean);
    if (!placed.length) return { width: 0, rows: [] };
    const track = placed[0].parentElement;
    const width = track.getBoundingClientRect().width;
    const trackLeft = track.getBoundingClientRect().left;
    const byTop = new Map();
    for (const el of placed) {
      const r = el.getBoundingClientRect();
      const key = Math.round(r.top);
      const row = byTop.get(key) ?? { right: 0, height: r.height };
      row.right = Math.max(row.right, r.right);
      byTop.set(key, row);
    }
    return {
      width,
      rows: [...byTop.values()].map((r) => ({
        fill: (r.right - trackLeft) / width,
        height: r.height,
      })),
    };
  });
  const fills = rows.rows.map((r) => `${(r.fill * 100).toFixed(0)}%`).join(" ");
  check(
    `no row overflows its ${rows.width.toFixed(0)}px track (${rows.rows.length} rows: ${fills})`,
    rows.rows.length > 0 && rows.rows.every((r) => r.fill <= 1.02),
  );
  const full = rows.rows.filter((r) => r.fill >= 0.98);
  const short = rows.rows.filter((r) => r.fill < 0.98);
  const tallestFull = Math.max(0, ...full.map((r) => r.height));
  const tallestShort = Math.max(0, ...short.map((r) => r.height));
  check(
    `a short row is not a tall row (${short.length} short, tallest ${tallestShort.toFixed(0)}px vs ${tallestFull.toFixed(0)}px full)`,
    short.length === 0 || tallestFull === 0 || tallestShort <= tallestFull * 1.7,
  );

  // The destination picker, opened the way a person opens it. Its sidebar is
  // the whole of finding C, and neither `tsc` nor a curl check can say whether
  // it renders -- the shortcuts come from the server but the layout does not.
  // The auto-tag panel from the check above is a full-screen overlay and will
  // swallow the click otherwise.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);
  await page.locator("img[src*='/thumb/']").first().click({ button: "right" });
  await page.waitForTimeout(300);
  const copyTo = page.locator("text=/^Copy .*to\\.\\.\\.$/").first();
  if (await copyTo.count()) {
    await copyTo.click();
    await page.waitForSelector("text=Parent folder", { timeout: 10_000 });
    const pinned = await page
      .locator("button[title^='/']")
      .evaluateAll((els) => els.map((e) => e.textContent.trim()));
    check(
      `the picker pins places (${pinned.join(", ") || "none"})`,
      pinned.includes("Gallery"),
    );
    // A shortcut that goes nowhere is worse than no shortcut, so the one the
    // server always knows must actually navigate.
    await page.locator("button[title^='/']", { hasText: "Gallery" }).first().click();
    await page.waitForTimeout(500);
    check(
      "clicking a pinned place navigates",
      (await page.locator("text=Parent folder").count()) > 0,
    );
    await page.keyboard.press("Escape");
    await page.waitForTimeout(200);
  } else {
    bad("the context menu had no copy-to entry, so the picker was unreachable");
  }

  // A phone-width viewport, which is the layout most likely to break silently.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(800);
  const mobileCells = await page.locator("img[src*='/thumb/']").count();
  check(`the grid re-lays out at phone width (${mobileCells} cells)`, mobileCells > 0);
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  check("nothing overflows horizontally at 390px", overflow <= 0);

  // Nothing is filtered out of either list. A 404 the page causes is a 404 a
  // user sees in their console, and "that one is fine" is how the missing
  // favicon link survived for as long as it did.
  check(
    `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 3).join(" | ")}` : ""}`,
    consoleErrors.length === 0,
  );
  check(
    `no failed requests${failedRequests.length ? `: ${failedRequests.slice(0, 5).join(" | ")}` : ""}`,
    failedRequests.length === 0,
  );
} catch (e) {
  bad(`threw: ${e}`);
  writeFileSync(join(work, "page.html"), await page.content().catch(() => ""));
  console.log(`  (page saved to ${join(work, "page.html")})`);
} finally {
  await browser.close();
  server.kill("SIGTERM");
}

console.log(`\n== ${passed} passed, ${failed} failed ==`);
if (stderr.trim()) console.log(`--- server stderr ---\n${stderr.trim()}`);
process.exit(failed === 0 ? 0 : 1);
