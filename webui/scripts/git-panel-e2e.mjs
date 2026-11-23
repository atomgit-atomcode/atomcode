// Real daemon + real Git writes through the source-control panel. No model calls.
// Run after: npm run build && cargo build -p atomcode-daemon --bin atomcode-daemon
// WORKBENCH_BROWSER may override the browser executable; WORKBENCH_DAEMON the binary.
import { chromium } from "playwright";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { spawn, execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { createServer } from "node:net";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const temp = await mkdtemp(resolve(tmpdir(), "atomcode-git-panel-"));
const root = resolve(temp, "project");
const home = resolve(temp, "home");
await mkdir(root);
await mkdir(home);

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
      "user.name=Git Panel Test",
      "-c",
      "user.email=git-panel@example.invalid",
      "-c",
      "core.autocrlf=false",
      ...args,
    ],
    {
      env: { ...process.env, HOME: home, USERPROFILE: home, GIT_CONFIG_NOSYSTEM: "1" },
      stdio: "pipe",
    },
  ).toString();

// Fixture: a committed baseline, then one edit, one staged add, one untracked
// file and one deletion — one row for each section the panel must show.
await writeFile(resolve(root, "tracked.txt"), "one\ntwo\n");
await writeFile(resolve(root, "gone.txt"), "delete me\n");
git(["init"]);
// Persisted, not per-command: the daemon runs `git commit` itself and needs the
// repository's own identity.
git(["config", "user.name", "Git Panel Test"]);
git(["config", "user.email", "git-panel@example.invalid"]);
git(["config", "commit.gpgSign", "false"]);
git(["config", "core.autocrlf", "false"]);
git(["add", "--", "."]);
git(["commit", "-m", "baseline"]);
await writeFile(resolve(root, "tracked.txt"), "one\ntwo changed\n");
await writeFile(resolve(root, "added.txt"), "staged content\n");
git(["add", "--", "added.txt"]);
await writeFile(resolve(root, "fresh.txt"), "untracked content\n");
await rm(resolve(root, "gone.txt"));

const portServer = createServer();
await new Promise((r) => portServer.listen(0, "127.0.0.1", r));
const port = portServer.address().port;
await new Promise((r) => portServer.close(r));
const base = "http://127.0.0.1:" + port;
const token = "git-panel-isolated-test-token-123456789";
const daemonBinary = process.env.WORKBENCH_DAEMON || resolve(repo, "target/debug/atomcode-daemon");
const binary =
  process.platform === "win32" && !daemonBinary.endsWith(".exe") && existsSync(daemonBinary + ".exe")
    ? daemonBinary + ".exe"
    : daemonBinary;
let failed = false;
const daemon = spawn(binary, ["--port", String(port), "--no-telemetry"], {
  cwd: root,
  env: {
    ...process.env,
    HOME: home,
    USERPROFILE: home,
    ATOMCODE_HOME: home,
    ATOMCODE_DAEMON_TOKEN: token,
  },
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
daemon.stdout.on("data", (b) => (log += b));
daemon.stderr.on("data", (b) => (log += b));
// Without this listener a missing binary emits an unhandled 'error' event,
// which skips the try/catch (and the cleanup) entirely.
daemon.on("error", (e) => {
  failed = true;
  log += `daemon did not start (${e.message}); build it with \`cargo build -p atomcode-daemon\` or point WORKBENCH_DAEMON at the binary\n`;
});

let browser;
async function api(path, body, method) {
  return fetch(base + path, {
    method: method || (body ? "POST" : "GET"),
    headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" },
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
}
async function until(fn, label = "") {
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
  throw new Error("Timed out waiting for " + (label || "the panel") + (last ? ": " + last.message : ""));
}
try {
  await until(async () => {
    if (daemon.exitCode !== null) throw new Error(log);
    return (await api("/health")).ok;
  }, "the daemon");

  const created = await api("/sessions", { working_dir: root, title: "Git panel" });
  assert.equal(created.status, 201, await created.clone().text());
  const session = await created.json();

  // Guard rails straight through HTTP: nothing may escape the workspace, no
  // option-shaped branch name, no empty commit.
  const stageBad = await api("/workbench/git/stage", {
    session_id: session.id,
    paths: [{ path: "../escape.txt" }],
  });
  assert.equal(stageBad.status, 400);
  const branchBad = await api("/workbench/git/branch", {
    session_id: session.id,
    name: "--force",
    create: false,
  });
  assert.equal(branchBad.status, 400);
  const commitEmpty = await api("/workbench/git/commit", {
    session_id: session.id,
    message: "   ",
  });
  assert.equal(commitEmpty.status, 400);

  const status = async () => {
    const response = await api("/workbench/git?session_id=" + session.id + "&stats=true");
    assert.equal(response.status, 200, await response.clone().text());
    return response.json();
  };
  const row = async (path) => (await status()).find((change) => change.path === path);

  browser = await chromium.launch({
    ...(process.env.WORKBENCH_BROWSER ? { executablePath: process.env.WORKBENCH_BROWSER } : {}),
    headless: true,
  });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  await page.addInitScript(() => {
    localStorage.setItem("atomcode.lang", "en");
    localStorage.setItem("atomcode.onboarding.done", "1");
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(base + "/?session=" + session.id + "&token=" + token);
  const toggle = page.locator(".workbench-toggle");
  await toggle.waitFor();
  await toggle.click();
  const panel = page.locator(".workbench");
  await panel.waitFor();
  await panel.locator("nav").getByRole("button", { name: "Git", exact: true }).click();
  await panel.locator(".workbench-git").waitFor();

  const stagedSection = panel.locator(".git-section").filter({ hasText: "Staged" }).first();
  const changesSection = panel.locator(".git-section").filter({ hasText: "Changes" }).first();
  const historySection = panel.locator(".git-section").filter({ hasText: "History" }).first();
  const rowIn = (section, path) => section.locator(".git-row").filter({ hasText: path }).first();

  await stagedSection.locator(".git-row").filter({ hasText: "added.txt" }).first().waitFor();
  await changesSection.locator(".git-row").filter({ hasText: "tracked.txt" }).first().waitFor();
  await changesSection.locator(".git-row").filter({ hasText: "fresh.txt" }).first().waitFor();
  await changesSection.locator(".git-row").filter({ hasText: "gone.txt" }).first().waitFor();
  assert.equal(await stagedSection.locator(".git-row").filter({ hasText: "tracked.txt" }).count(), 0);

  // Selecting a change shows its line diff in the pane beside the list.
  await rowIn(changesSection, "tracked.txt").locator(".git-row-main").click();
  await until(async () =>
    (await panel.locator(".workbench-git-diff .workbench-diff").textContent()).includes("+two changed"),
  "the working-tree diff");

  // Stage one file from its row: it moves to the staged section.
  await rowIn(changesSection, "tracked.txt").locator('.git-row-actions button[title="Stage"]').click();
  await until(async () => (await row("tracked.txt"))?.index === "M", "the index to take the edit");
  await until(async () =>
    (await rowIn(stagedSection, "tracked.txt").count()) === 1,
  "the row to move to Staged");

  // Commit through the UI and verify Git really recorded it.
  await panel.locator(".git-commit-box textarea").fill("feat: commit from the panel");
  await until(async () => panel.locator(".git-commit-button").isEnabled(), "the commit button");
  await panel.locator(".git-commit-button").click();
  await until(async () => {
    if (git(["log", "-1", "--pretty=%s"]).trim() === "feat: commit from the panel") return true;
    const banner = panel.locator(".workbench-error").first();
    if (await banner.count()) {
      // Surface the panel's own message instead of a bare timeout.
      throw new Error("commit rejected: " + (await banner.textContent()));
    }
    return false;
  }, "the commit");
  assert.equal((await status()).length, 2, "only fresh.txt and gone.txt should be left");
  await historySection.locator("button").filter({ hasText: "feat: commit from the panel" }).first().waitFor();

  // History detail: the commit's files and diff are shown read-only.
  await historySection
    .locator("button")
    .filter({ hasText: "feat: commit from the panel" })
    .first()
    .click();
  await panel.locator(".git-commit-detail").waitFor();
  await until(async () =>
    (await panel.locator(".git-commit-files").textContent()).includes("tracked.txt"),
  "the commit file list");

  // Unstage everything, then commit nothing: the index is empty again.
  await changesSection.locator(".git-row").filter({ hasText: "gone.txt" }).first()
    .locator('.git-row-actions button[title="Stage"]').click();
  await until(async () => (await row("gone.txt"))?.index === "D", "the staged deletion");
  await stagedSection.locator(".git-section-action").click();
  await until(async () => git(["diff", "--cached", "--name-only"]).trim() === "", "the index to clear");

  // Discarding an untracked file asks first, then removes it from disk.
  await rowIn(changesSection, "fresh.txt").locator('.git-row-actions button[title="Discard changes"]').click();
  const dialog = page.locator(".modal-overlay, dialog, .confirm-dialog").first();
  await until(async () => (await dialog.count()) > 0, "the discard confirmation");
  await page.getByRole("button", { name: "Discard", exact: true }).click();
  await until(() => !existsSync(resolve(root, "fresh.txt")), "the discarded file to disappear");
  await until(async () => !(await status()).some((change) => change.path === "fresh.txt"), "the row to vanish");

  // Branch creation from the toolbar switches the repository branch.
  await panel.locator(".git-branch-button").click();
  await panel.locator(".git-branch-create input").fill("panel-test");
  await panel.locator(".git-branch-create button").click();
  await until(() => git(["branch", "--show-current"]).trim() === "panel-test", "the branch switch");
  await until(async () =>
    (await panel.locator(".git-branch-name").textContent()).includes("panel-test"),
  "the toolbar branch name");

  // The screenshot is the only visual evidence of this run: keep it outside the
  // fixture directory, which the cleanup below removes.
  const shot = resolve(tmpdir(), "atomcode-git-panel.png");
  await page.screenshot({ path: shot, fullPage: true });
  console.log("screenshot:", shot);
  assert.deepEqual(errors, [], "no page errors");
  console.log("git-panel e2e passed");
} catch (e) {
  failed = true;
  console.error("git-panel e2e failed:", e.message);
  console.error(log.slice(-3000));
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
  daemon.kill();
  await new Promise((done) => {
    if (daemon.exitCode !== null) return done();
    daemon.once("exit", done);
    setTimeout(done, 5000);
  });
  if (failed) {
    // Keep the repository as it was left, so the failing state can be inspected.
    console.error("fixture kept at:", temp);
  } else {
    // Windows keeps a handle on the daemon's cwd for a moment after exit.
    for (let attempt = 0; attempt < 10; attempt++) {
      try {
        await rm(temp, { recursive: true, force: true });
        break;
      } catch {
        await new Promise((r) => setTimeout(r, 200));
      }
    }
  }
}
