
import { chromium } from "playwright";
const BASE = "http://127.0.0.1:7878";
const browser = await chromium.launch({ channel: "chrome", headless: true });
const ctx = await browser.newContext({
  viewport: { width: 1400, height: 860 },
  recordVideo: { dir: "/tmp/vibery-demo/out", size: { width: 1400, height: 860 } },
});
const page = await ctx.newPage();
const sleep = (ms) => page.waitForTimeout(ms);

async function caption(html) {
  await page.evaluate((html) => {
    let el = document.getElementById("__cap");
    if (!el) {
      el = document.createElement("div");
      el.id = "__cap";
      el.style.cssText = "position:fixed;left:50%;bottom:22px;transform:translateX(-50%);z-index:99999;" +
        "background:rgba(23,23,23,.88);color:#fff;font:500 15px/1.45 Inter,-apple-system,sans-serif;" +
        "padding:10px 18px;border-radius:10px;max-width:900px;text-align:center;box-shadow:0 6px 24px rgba(0,0,0,.25);pointer-events:none";
      document.body.appendChild(el);
    }
    el.innerHTML = html;
  }, html);
}
const code = (s) => `<code style="background:#ffffff22;padding:1px 6px;border-radius:4px;font-family:SFMono-Regular,Menlo,monospace;color:#ffd7b5">${s}</code>`;

async function meta() {
  await sleep(250);
  return (await page.locator(".results-meta").innerText()).replace(/\s+/g, " ").replace("Syntax ?", "").replace("Hide syntax", "").trim();
}

async function query(q, explain, { clear = true, hold = 2600 } = {}) {
  const focused = await page.evaluate(() => document.activeElement?.tagName === "INPUT");
  if (!focused) await page.keyboard.press("/");
  const box = page.locator(".searchbox input");
  if (clear) {
    for (let i = 0; i < 5 && (await box.inputValue()) !== ""; i++) { await box.fill(""); await sleep(120); }
  }
  await caption(explain);
  await page.keyboard.type(q, { delay: 75 });
  const m = await meta();
  await caption(`${explain}<br><span style="color:#9fe0a8">${m}</span>`);
  await sleep(hold);
}

async function openTimed(action, label) {
  const t0 = Date.now();
  await action();
  try { await page.locator("h1.entity-title").first().waitFor({ timeout: 8000 }); }
  catch (e) { await page.screenshot({ path: "/tmp/vibery-demo/fail.png" }); console.log("URL", page.url()); throw e; }
  await page.waitForFunction(() => document.querySelector(".md"));
  const ms = Date.now() - t0;
  await caption(`${label} <span style="color:#9fe0a8">opened in ${ms} ms</span> (click → rendered, measured by the test driver)`);
  return ms;
}

try {
// 1. Home
await page.goto(BASE + "/");
await page.locator(".hit").first().waitFor();
await caption("<b>vibery</b>: a local Fibery front end. 3.2M entities, Rust server + React. Everything below is real time.");
await sleep(3500);

// 2. Search as you type
await query("rvo", `Search updates on every keystroke. ${code("/")} focuses the search bar`);
await query(" #bet", `${code("#bet")} filters by database straight from the keyboard. No dropdowns`, { clear: false });
await query("rvo #bet is:open", `${code("is:open")} uses the workflow's own open/closed flag`);
await query("subdivide", `Stemming + fuzzy: ${code("subdivide")} finds “subdivision”, “subdivided”`);
await query("DataView", `camelCase is split: ${code("DataView")} finds “Data Views”. Curated pins put the canonical doc first`);
await query("by:@sami sort:recent", `People filters work with no text at all: ${code("by:@sami")} ${code("sort:recent")}`);
await query("commented-by:@tokar #scope", `${code("commented-by:")}, ${code("assignees:")}, ${code("touched-by:")}. Any user field works`);
await query("#note related:(#team platform) interventions",
  `Relations: ${code("related:( … )")} takes a subquery. Notes linked to the Platform team that mention interventions`, { hold: 3200 });
await query('"load flow" -#issue updated:-90d', `Phrases, exclusion and time windows: ${code('"load flow"')} ${code("-#issue")} ${code("updated:-90d")}`);

// 3. Facet click
await query("rvo", "Database counts are clickable…", { hold: 1500 });
await page.locator(".facet", { hasText: "Engineering/Scope" }).first().click({ timeout: 5000 });
await caption(`…and add ${code("#Engineering/Scope")} to the query <span style="color:#9fe0a8">${await meta()}</span>`);
await sleep(2200);

// 4. Syntax panel
await page.getByRole("button", { name: /Syntax/ }).click();
await caption("The full syntax reference is one click away");
await sleep(3000);
await page.getByRole("button", { name: /Hide syntax/ }).click();

// 5. Keyboard nav + open notes fast
await query("#note rvo", "Arrow keys move through results; the selected result is prefetched", { hold: 800 });
for (let i = 0; i < 3; i++) { await page.keyboard.press("ArrowDown"); await sleep(350); }
await openTimed(() => page.keyboard.press("Enter"), "↵ Enter:");
await sleep(2500);
await page.mouse.wheel(0, 700); await sleep(900); await page.mouse.wheel(0, 700); await sleep(1200);
await page.goBack(); await page.locator(".hit").first().waitFor(); await sleep(600);
for (const n of [5, 8, 2]) {
  await openTimed(() => page.locator(".hit").nth(n).click(), "Click:");
  await sleep(1300);
  await page.goBack(); await page.locator(".hit").first().waitFor(); await sleep(400);
}

// 6. Rich markdown: callouts, tables, highlights
await openTimed(() => page.goto(BASE + "/Shared/Note/rvo-product-one-pager-8906"), "Fibery URLs work as-is (cold page load):");
await sleep(1800);
await caption("Proper markdown: Fibery callouts, headings, lists, inline formatting");
await page.mouse.wheel(0, 600); await sleep(1800);
await openTimed(() => page.goto(BASE + "/Shared/Note/pge-rvo-milestones-15792"), "Tables with Fibery highlight colours:");
await sleep(1500);
await page.mouse.wheel(0, 450); await sleep(2200);

// 7. Mentions as links, hop between notes
await page.goto(BASE + "/e/33bc820d-5404-422c-8e59-ef2e17738467");
await page.locator("h1.entity-title").waitFor();
await caption("Fibery mentions become real links with resolved titles");
await sleep(2000);
for (const t of ["Tom 2026-10-12", "Tom 2026-09-07"]) {
  await openTimed(() => page.locator(".md a", { hasText: t }).first().click(), `Follow mention “${t}”:`);
  await sleep(1600);
  await page.goBack(); await page.locator("h1.entity-title").waitFor(); await sleep(500);
}

// 8. Comments + backlinks
await openTimed(() => page.goto(BASE + "/Shared/Note/fibery-search-issues-27735"), "The original “fibery search issues” note:");
await sleep(1500);
await page.locator(".comments").scrollIntoViewIfNeeded();
await caption("Comments render inline, so Ctrl-F covers the whole discussion");
await sleep(3000);

// 9. Editor (not saved)
await page.evaluate(() => window.scrollTo(0, 0)); await sleep(500);
await page.getByRole("button", { name: "Edit" }).first().click();
await page.locator(".editor textarea").waitFor();
await caption("Edit markdown with a live preview. ⌘S saves (local by default). Not saved in this demo");
const ta = page.locator(".editor textarea");
await ta.press("Meta+ArrowUp");
await ta.type("> [//]: # (callout;icon-type=icon;icon=:bulb:;color=#bf5b0f)\n> **Demo:** this callout is typed live\n\n| query | what |\n| --- | --- |\n| `#bet rvo` | bets about RVO |\n\n", { delay: 18 });
await sleep(2500);
await page.getByRole("button", { name: "Done" }).click();
await sleep(600);

await caption("<b>vibery</b>: median search ~1–8 ms on the server, notes open in a few ms. Code in <code>vibery/web/</code>");
await sleep(3000);

} catch (e) {
  console.log("FAIL", page.url(), e.message.split("\n")[0]);
  await page.screenshot({ path: "/tmp/vibery-demo/fail.png" });
  await ctx.close(); await browser.close(); process.exit(1);
}
await ctx.close();
await browser.close();
console.log("video:", await page.video().path());
