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
