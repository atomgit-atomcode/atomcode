// Real daemon + real PTY browser smoke. No model/provider calls.
// Run after: npm run build && cargo build -p atomcode-daemon --bin atomcode-daemon
// WORKBENCH_BROWSER may override the Chrome executable; WORKBENCH_DAEMON the binary.
import { chromium } from "playwright";
import { mkdtemp, mkdir, writeFile, rm, readFile } from "node:fs/promises";
import { spawn, execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { createServer } from "node:net";
const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const temp = await mkdtemp(resolve(tmpdir(), "atomcode-workbench-"));
const root = resolve(temp, "project");
const other = resolve(temp, "other");
await mkdir(root);
await mkdir(other);
await mkdir(resolve(temp, "home"));
await writeFile(
  resolve(root, "README.md"),
  "# Workbench fixture\n\nRead-only **Markdown** preview.",
);
await writeFile(resolve(root, "hello.ts"), "export const answer = 42;");
await writeFile(resolve(root, "data.json"), '{"answer":42}');
await writeFile(
  resolve(root, "large.txt"),
  Buffer.alloc(2 * 1024 * 1024 + 1, 65),
);
await writeFile(
  resolve(root, "pixel.png"),
  Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=",
    "base64",
  ),
);
await writeFile(resolve(temp, "outside.txt"), "outside workspace");
await writeFile(resolve(other, "only-other.txt"), "second session");
const git = (args) =>
  execFileSync(
    "git",
    [
      "-C",
      root,
      "-c",
      "core.hooksPath=" + resolve(temp, "no-hooks"),
      "-c",
      "commit.gpgSign=false",
      "-c",
      "user.name=Workbench Test",
      "-c",
      "user.email=workbench@example.invalid",
      ...args,
    ],
    {
      env: {
        ...process.env,
        HOME: resolve(temp, "home"),
        GIT_CONFIG_NOSYSTEM: "1",
      },
      stdio: "pipe",
    },
  );
await writeFile(resolve(root, "windows.txt"), "first\r\nsecond\r\n");
await writeFile(
  resolve(root, "long.txt"),
  Array.from({ length: 300 }, (_, i) => "Line " + i).join("\n"),
);
git(["init"]);
git(["add", "--", "."]);
git(["commit", "-m", "baseline"]);
const portServer = createServer();
await new Promise((r) => portServer.listen(0, "127.0.0.1", r));
const port = portServer.address().port;
await new Promise((r) => portServer.close(r));
const base = "http://127.0.0.1:" + port;
const token = "workbench-isolated-test-token-123456789";
const daemon = spawn(
  process.env.WORKBENCH_DAEMON || resolve(repo, "target/debug/atomcode-daemon"),
  ["--port", String(port), "--no-telemetry"],
  {
    cwd: root,
    env: {
      ...process.env,
      HOME: resolve(temp, "home"),
      USERPROFILE: resolve(temp, "home"),
      ATOMCODE_HOME: resolve(temp, "home"),
      ATOMCODE_DAEMON_TOKEN: token,
    },
    stdio: ["ignore", "pipe", "pipe"],
  },
);
let log = "";
daemon.stdout.on("data", (b) => (log += b));
daemon.stderr.on("data", (b) => (log += b));
let browser;
async function api(path, body, method) {
  const r = await fetch(base + path, {
    method: method || (body ? "POST" : "GET"),
    headers: {
      Authorization: "Bearer " + token,
      "Content-Type": "application/json",
    },
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
  return r;
}
async function until(fn) {
  let last;
  for (let i = 0; i < 100; i++) {
    try {
      const result = await fn();
      if (result) return result;
    } catch (e) {
      last = e;
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw last || new Error("Timed out");
}
try {
  await until(async () => {
    if (daemon.exitCode !== null) throw new Error(log);
    return (await api("/health")).ok;
  });
  const create = async (path, title) => {
    const r = await api("/sessions", { working_dir: path, title });
    assert.equal(r.status, 201, await r.clone().text());
    return r.json();
  };
  const a = await create(root, "Workbench A");
  const b = await create(other, "Workbench B");
  assert.equal(
    (await fetch(base + "/workbench/files?session_id=" + a.id)).status,
    401,
  );
  const query = (path) => new URLSearchParams({ session_id: a.id, path });
  assert.equal(
    (await api("/workbench/preview?" + query("../outside.txt"))).status,
    400,
  );
  assert.equal(
    (await api("/workbench/preview?" + query("large.txt"))).status,
    400,
  );
  browser = await chromium.launch({
    executablePath:
      process.env.WORKBENCH_BROWSER ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 1000 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(base + "/?session=" + a.id + "&token=" + token);
  const toggle = page.locator(".workbench-toggle");
  await toggle.waitFor();
  await toggle.click();
  const panel = page.locator(".workbench");
  await panel.waitFor();
  await panel
    .getByRole("button", { name: "README.md", exact: false })
    .first()
    .click();
  await panel.getByRole("heading", { name: "Workbench fixture" }).waitFor();
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "hello.ts", exact: false })
    .click();
  await until(async () =>
    (await panel.locator(".workbench-preview").textContent()).includes(
      "answer = 42",
    ),
  );
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "data.json", exact: false })
    .click();
  await until(async () =>
    (await panel.locator(".workbench-preview").textContent()).includes(
      '"answer":42',
    ),
  );
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "pixel.png", exact: false })
    .click();
  await panel.locator(".workbench-preview img").waitFor();
  await until(() =>
    panel
      .locator(".workbench-preview img")
      .evaluate((img) => img.naturalWidth > 0),
  );
  await writeFile(resolve(root, "refresh-added.txt"), "new file");
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await panel.locator(".workbench-tree").getByRole("button", { name: "refresh-added.txt", exact: false }).waitFor();
  await page.evaluate(({session, path}) => window.dispatchEvent(new CustomEvent("workbench-open-file", {detail:{session,path}})), {session:a.id,path:resolve(root,"hello.ts")});
  await until(async () => (await panel.locator(".workbench-preview").textContent()).includes("answer = 42"));
  await page.screenshot({ path: resolve(temp, "files.png"), fullPage: true });
  await panel.locator("nav button").nth(1).click();
  await panel.locator(".workbench-terminal-actions button").click();
  await panel.locator(".xterm-helper-textarea").waitFor();
  await panel.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type(
    "printf '\\120\\124\\131_WORKBENCH_OK\\n'; pwd; echo $$ > shell.pid; sleep 60 & echo $! > child.pid",
  );
  await page.keyboard.press("Enter");
  await until(async () =>
    (await panel.locator(".xterm-screen").textContent()).includes(
      "PTY_WORKBENCH_OK",
    ),
  );
  await until(() =>
    page.evaluate(
      (id) =>
        JSON.parse(sessionStorage.getItem("workbench:" + id) || "null")
          ?.terminals?.length === 1,
      a.id,
    ),
  );
  const ids = await page.evaluate(
    (id) => JSON.parse(sessionStorage.getItem("workbench:" + id)).terminals,
    a.id,
  );
  assert.equal(ids.length, 1);
  const terminal = ids[0];
  // Resize a real PTY and verify the shell sees the requested dimensions.
  assert.equal(
    (
      await api("/workbench/terminals/" + terminal, {
        session_id: a.id,
        rows: 35,
        cols: 100,
        input: "stty size > dimensions.txt\r",
      })
    ).status,
    204,
  );
  await until(
    async () =>
      (await readFile(resolve(root, "dimensions.txt"), "utf8")).trim() ===
      "35 100",
  );
  const extras = [];
  for (let i = 0; i < 3; i++) {
    const r = await api("/workbench/terminals", { session_id: a.id });
    assert.equal(r.status, 200);
    extras.push((await r.json()).id);
  }
  assert.equal(
    (await api("/workbench/terminals", { session_id: a.id })).status,
    400,
  );
  for (const id of extras)
    assert.equal(
      (
        await api(
          "/workbench/terminals/" + id + "?session_id=" + a.id,
          undefined,
          "DELETE",
        )
      ).status,
      204,
    );
  assert.equal(
    (
      await api("/workbench/terminals/" + terminal, {
        session_id: a.id,
        input: "x".repeat(16385),
      })
    ).status,
    400,
  );
  let response = await api(
    "/workbench/terminals/" + terminal + "?session_id=" + b.id,
  );
  assert.equal(response.status, 404);
  await page.screenshot({ path: resolve(temp, "desktop.png"), fullPage: true });
  await page.goto(base + "/?session=" + b.id + "&token=" + token);
  await page.locator(".workbench-toggle").click();
  assert.equal(await page.locator(".workbench-tabs").textContent(), "");
  await page
    .locator(".workbench-tree")
    .getByRole("button", { name: "only-other.txt", exact: false })
    .waitFor();
  await page.goto(base + "/?session=" + a.id + "&token=" + token);
  await page.locator(".workbench-toggle").click();
  await page.locator(".xterm-helper-textarea").waitFor();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: resolve(temp, "mobile.png"), fullPage: true });
  assert.equal(
    await page
      .locator(".workbench")
      .evaluate((el) => el.getBoundingClientRect().width <= innerWidth),
    true,
  );
  await page.locator(".workbench-tabs div button").last().click();
  assert.equal(
    (await api("/workbench/terminals/" + terminal + "?session_id=" + a.id))
      .status,
    404,
  );
  for (const name of ["shell.pid", "child.pid"]) {
    const pid = Number((await readFile(resolve(root, name), "utf8")).trim());
    await until(() => {
      try {
        process.kill(pid, 0);
        return false;
      } catch (e) {
        return e.code === "ESRCH";
      }
    });
  }
  await panel.locator(".workbench-terminal-actions button").click();
  await page.locator(".xterm-helper-textarea").waitFor();
  await page.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type("exit 7");
  await page.keyboard.press("Enter");
  await page.getByRole("button", { name: /重新启动终端|Restart terminal/ }).waitFor();
  await until(async () => (await page.locator(".xterm").textContent()).includes(": 7]"));
  await page.getByRole("button", { name: /重新启动终端|Restart terminal/ }).click();
  await page.locator(".xterm-helper-textarea").waitFor();
  await page.locator(".workbench-tabs div button").last().click();
  await page.locator(".workbench header button").click();
  await page.locator(".workbench-toggle").waitFor();
  // The initial landing page can open the workbench without an LLM round-trip.
  await page.goto(base + "/?token=" + token);
  await page.locator(".workbench-toggle").click();
  await page
    .locator(".workbench-tree")
    .getByRole("button", { name: "only-other.txt", exact: false })
    .waitFor();
  assert.ok(new URL(page.url()).searchParams.get("session"));
  await page.locator(".workbench header button").click();

  // Phase 2: real editing, conflict detection, draft protection and Git diffs.
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto(base + "/?session=" + a.id + "&token=" + token);
  await page.locator(".workbench-toggle").click();
  await panel.locator("nav button").nth(0).click();
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "hello.ts", exact: false })
    .click();
  await panel.locator(".workbench-file-mode button").nth(1).click();
  const editor = panel.getByRole("textbox", { name: "文件编辑器" });
  await editor.waitFor();
  await editor.fill("export const answer = 43;\n");
  assert.equal(
    await panel
      .locator(".workbench-tabs")
      .textContent()
      .then((t) => t.includes("●")),
    true,
  );
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "README.md", exact: false })
    .click();
  await panel.locator('.workbench-tabs button[title="hello.ts"]').click();
  assert.equal(await editor.inputValue(), "export const answer = 43;\n");
  await panel.locator(".workbench-save").click();
  await until(
    async () =>
      (await readFile(resolve(root, "hello.ts"), "utf8")) ===
      "export const answer = 43;\n",
  );
  await until(async () => await panel.locator(".workbench-save").isDisabled());
  // The server commits one version while the user continues typing.
  let reply;
  let release;
  let delayed = new Promise((resolve) => (release = resolve));
  await page.route("**/workbench/text", async (route) => {
    if (route.request().method() === "POST") {
      reply = await route.fetch();
      await delayed;
      await route.fulfill({ response: reply });
    } else await route.continue();
  });
  await editor.fill("export const answer = 44;\n");
  await panel.locator(".workbench-save").click();
  await until(() => !!reply);
  await editor.fill("export const answer = 45;\n");
  release();
  await until(async () => await panel.locator(".workbench-save").isEnabled());
  assert.equal(await editor.inputValue(), "export const answer = 45;\n");
  await page.unroute("**/workbench/text");
  await editor.press("Control+s");
  await until(
    async () =>
      (await readFile(resolve(root, "hello.ts"), "utf8")) ===
      "export const answer = 45;\n",
  );
  await until(async () => await panel.locator(".workbench-save").isDisabled());
  // External writes must win; the editor retains its rejected draft.
  await editor.fill("unsaved conflict draft\n");
  await writeFile(resolve(root, "hello.ts"), "external writer\n");
  await panel.locator(".workbench-save").click();
  await until(async () =>
    (
      await panel.locator('.workbench-editor [role="alert"]').textContent()
    ).includes("外部修改"),
  );
  assert.equal(
    await readFile(resolve(root, "hello.ts"), "utf8"),
    "external writer\n",
  );
  assert.equal(await editor.inputValue(), "unsaved conflict draft\n");
  page.once("dialog", (dialog) => dialog.dismiss());
  await panel
    .getByRole("button", { name: "关闭 hello.ts", exact: true })
    .click();
  assert.equal(await editor.isVisible(), true);
  let releaseReload;
  const reloadGate = new Promise(resolve => { releaseReload = resolve; });
  await page.route("**/workbench/text?*", async route => {
    const response = await route.fetch();
    await reloadGate;
    await route.fulfill({ response });
  });
  page.once("dialog", (dialog) => dialog.accept());
  await panel
    .locator(".workbench-editor-actions")
    .getByRole("button", { name: "重新载入", exact: true })
    .click();
  await until(() => editor.evaluate(el => el.readOnly));
  await editor.focus();
  await page.keyboard.type("must not overwrite reload");
  assert.equal(await editor.inputValue(), "unsaved conflict draft\n");
  releaseReload();
  await until(async () => (await editor.inputValue()) === "external writer\n");
  await page.unroute("**/workbench/text?*");
  await until(() => editor.evaluate(el => !el.readOnly));
  await editor.fill("session A retained draft\n");
  page.once("dialog", (dialog) => dialog.accept());
  await panel.locator("header button").click();
  await page.locator(".workbench-toggle").click();
  assert.equal(await editor.inputValue(), "session A retained draft\n");
  await panel.getByRole("separator").focus();
  await page.keyboard.press("ArrowLeft");
  const panelWidth = await panel.evaluate((el) =>
    Math.round(el.getBoundingClientRect().width),
  );
  await until(() =>
    page.evaluate(
      (id) =>
        JSON.parse(sessionStorage.getItem("workbench:" + id)).drafts["hello.ts"]
          ?.text === "session A retained draft\n",
      a.id,
    ),
  );
  page.once("dialog", (dialog) => dialog.accept());
  await page.goto(base + "/?session=" + b.id + "&token=" + token);
  await page.locator(".workbench-toggle").click();
  assert.equal(
    await panel
      .locator(".workbench-tabs")
      .textContent()
      .then((t) => t.includes("hello.ts")),
    false,
  );
  page.once("dialog", (dialog) => dialog.accept());
  await page.goto(base + "/?session=" + a.id + "&token=" + token);
  await page.locator(".workbench-toggle").click();
  assert.equal(await editor.inputValue(), "session A retained draft\n");
  assert.equal(
    await panel.evaluate((el) => Math.round(el.getBoundingClientRect().width)),
    panelWidth,
  );
  await editor.press("Control+s");
  await until(
    async () =>
      (await readFile(resolve(root, "hello.ts"), "utf8")) ===
      "session A retained draft\n",
  );
  await until(async () => await panel.locator(".workbench-save").isDisabled());
  await page.screenshot({ path: resolve(temp, "editor.png"), fullPage: true });
  // Textarea normalization must not silently rewrite CRLF files to LF.
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "windows.txt", exact: false })
    .click();
  await panel.locator(".workbench-file-mode button").nth(1).click();
  await editor.waitFor();
  await editor.fill("first\nsecond\nthird\n");
  await panel.locator(".workbench-save").click();
  await until(
    async () =>
      (await readFile(resolve(root, "windows.txt"), "utf8")) ===
      "first\r\nsecond\r\nthird\r\n",
  );
  await until(async () => await panel.locator(".workbench-save").isDisabled());
  // Keep caret and scroll position when switching file tabs.
  await panel
    .locator(".workbench-tree")
    .getByRole("button", { name: "long.txt", exact: false })
    .click();
  await panel.locator(".workbench-file-mode button").nth(1).click();
  await editor.waitFor();
  await editor.evaluate((el) => {
    el.focus();
    el.setSelectionRange(40, 40);
    el.scrollTop = 220;
    el.dispatchEvent(new Event("select", { bubbles: true }));
    el.dispatchEvent(new Event("scroll", { bubbles: true }));
  });
  await panel.locator('.workbench-tabs button[title="hello.ts"]').click();
  await panel.locator('.workbench-tabs button[title="long.txt"]').click();
  assert.equal(await editor.evaluate((el) => el.selectionStart), 40);
  assert.ok(await editor.evaluate((el) => el.scrollTop >= 200));
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(
    await editor.evaluate(
      (el) => el.getBoundingClientRect().width <= innerWidth,
    ),
  );
  for (const selector of [
    ".workbench",
    ".workbench header button",
    ".workbench-save",
  ]) {
    assert.ok(
      await page.locator(selector).evaluate((el) => {
        const r = el.getBoundingClientRect();
        return r.left >= 0 && r.right <= innerWidth;
      }),
      selector + " escaped the mobile viewport",
    );
  }
  await page.screenshot({
    path: resolve(temp, "editor-mobile.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 1440, height: 1000 });
  // Git view can inspect but must not change the index or HEAD.
  await writeFile(resolve(root, "data.json"), '{"answer":43}');
  git(["add", "--", "data.json"]);
  await writeFile(resolve(root, "new file.txt"), "untracked fixture\n");
  const indexBefore = await readFile(resolve(root, ".git/index"));
  const headBefore = git(["rev-parse", "HEAD"]).toString();
  await panel
    .locator("nav")
    .getByRole("button", { name: "Git", exact: true })
    .click();
  await panel
    .locator(".workbench-changes button")
    .filter({ hasText: "hello.ts" })
    .click();
  await until(async () =>
    (await panel.locator(".workbench-diff").textContent()).includes(
      "+session A retained draft",
    ),
  );
  await panel
    .locator(".workbench-changes button")
    .filter({ hasText: "data.json" })
    .click();
  await until(async () =>
    (await panel.locator(".workbench-diff").textContent()).includes(
      '+{"answer":43}',
    ),
  );
  await panel
    .locator(".workbench-changes button")
    .filter({ hasText: "new file.txt" })
    .click();
  await until(async () =>
    (await panel.locator(".workbench-diff").textContent()).includes(
      "+untracked fixture",
    ),
  );
  assert.deepEqual(await readFile(resolve(root, ".git/index")), indexBefore);
  assert.equal(git(["rev-parse", "HEAD"]).toString(), headBefore);
  await page.screenshot({ path: resolve(temp, "git.png"), fullPage: true });
  await page.evaluate(() => (document.documentElement.dataset.theme = "dark"));
  await page.screenshot({
    path: resolve(temp, "git-dark.png"),
    fullPage: true,
  });
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify(
      {
        passed: true,
        artifacts: temp,
        checks: [
          "authentication",
          "workspace boundary",
          "preview limit",
          "Markdown",
          "text",
          "image",
          "JSON",
          "landing-page workspace",
          "real PTY",
          "PTY resize",
          "terminal limits",
          "process tree cleanup",
          "session isolation",
          "reconnect",
          "mobile",
          "terminal close",
          "edit/save/keyboard shortcut",
          "typing during save",
          "external conflict",
          "dirty close protection",
          "session drafts and panel width",
          "CRLF preservation",
          "caret and scroll restoration",
          "mobile editor",
          "dark Git view",
          "read-only working/staged/untracked Git diffs",
        ],
      },
      null,
      2,
    ),
  );
} catch (e) {
  console.error(log.slice(-3000));
  throw e;
} finally {
  if (browser) await browser.close();
  daemon.kill("SIGTERM");
  await Promise.race([
    new Promise((r) => daemon.once("exit", r)),
    new Promise((r) => setTimeout(r, 3000)),
  ]);
}
