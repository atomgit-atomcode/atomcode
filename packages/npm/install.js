#!/usr/bin/env node
/**
 * AtomCode npm installer — postinstall hook
 *
 * Downloads the correct pre-compiled binary from GitCode Releases
 * for the current platform and places it in the package's bin/ directory.
 *
 * Environment:
 *   ATOMCODE_VERSION   release tag to download (e.g. "v4.23.1")
 *                      (default: npm_package_version prefixed with "v")
 *   ATOMCODE_DOWNLOAD_URL  override the entire download URL base
 *                      (default: GitCode Releases API)
 */

"use strict";

const fs = require("fs");
const path = require("path");
const https = require("https");

const PKG_VERSION = process.env.npm_package_version;
const VERSION = process.env.ATOMCODE_VERSION || `v${PKG_VERSION}`;

// ── platform detection ──────────────────────────────────────────────
const OS_MAP = {
  darwin: "darwin",
  linux: "linux",
  win32: "windows",
  ohos: "ohos",
};
const ARCH_MAP = {
  arm64: "arm64",
  x64: "x64",
};

const os = OS_MAP[process.platform];
const arch = ARCH_MAP[process.arch];
if (!os || !arch) {
  console.error(
    `Unsupported platform: ${process.platform}-${process.arch}. ` +
      "AtomCode provides binaries for darwin (arm64/x64), linux (arm64/x64), windows (x64), and ohos (arm64)."
  );
  process.exit(1);
}

const BIN_SUFFIX = os === "windows" ? ".exe" : "";
const TARGET_TAG = `${os}-${arch}`;

// ── download DOWNLOAD_URL ────────────────────────────────────────────────────
const REPO_BASE =
  process.env.ATOMCODE_DOWNLOAD_URL ||
  "https://atomgit.com/atomgit_atomcode/atomcode/releases/download";
const BIN_NAME = `atomcode-${VERSION}-${TARGET_TAG}${BIN_SUFFIX}`;
const DOWNLOAD_URL = `${REPO_BASE}/${VERSION}/${BIN_NAME}`;

// ── paths ───────────────────────────────────────────────────────────
const BIN_DIR = path.join(__dirname, "bin");
const BIN_PATH = path.join(BIN_DIR, `atomcode${BIN_SUFFIX}`);

// ── helpers ─────────────────────────────────────────────────────────
const MAX_REDIRECTS = 5;

function download(url, destPath, redirectCount) {
  if (redirectCount === undefined) redirectCount = 0;
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(destPath);
    https
      .get(url, { headers: { "User-Agent": "atomcode-npm-installer" } }, (res) => {
        // Follow redirects (GitCode may redirect to CDN)
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          file.close(() => {
            fs.unlinkSync(destPath);
            if (redirectCount >= MAX_REDIRECTS) {
              return reject(new Error(`Too many redirects (${MAX_REDIRECTS})`));
            }
            download(new URL(res.headers.location, url).href, destPath, redirectCount + 1).then(resolve, reject);
          });
          return;
        }
        if (res.statusCode !== 200) {
          file.close(() => {
            fs.unlinkSync(destPath);
            reject(
              new Error(`HTTP ${res.statusCode} ${res.statusMessage}\n  DOWNLOAD_URL: ${url}`)
            );
          });
          return;
        }
        res.pipe(file);
        file.on("finish", () => {
          file.close(() => resolve());
        });
      })
      .on("error", (err) => {
        file.close(() => {
          if (fs.existsSync(destPath)) fs.unlinkSync(destPath);
          reject(err);
        });
      });
  });
}

// ── main ────────────────────────────────────────────────────────────
async function main() {
  console.log(`\n  ╔══════════════════════════════════════════════╗`);
  console.log(`  ║           AtomCode Installer                  ║`);
  console.log(`  ╚══════════════════════════════════════════════╝\n`);

  console.log(`  → Platform: ${TARGET_TAG}`);
  console.log(`  → Version:  ${VERSION}`);
  console.log(`  → Download: ${DOWNLOAD_URL}\n`);

  // Ensure bin/ directory exists
  fs.mkdirSync(BIN_DIR, { recursive: true });

  // Check if already installed
  if (fs.existsSync(BIN_PATH)) {
    console.log(`  ✓ Binary already exists at ${BIN_PATH}`);
    console.log(`    Run 'npm rebuild @atomgit/atomcode' to re-download.\n`);
    return;
  }

  // Download
  try {
    console.log("  ⏳ Downloading...");
    await download(DOWNLOAD_URL, BIN_PATH);
  } catch (err) {
    console.error(`\n  ✗ Download failed:`);
    console.error(`    ${err.message}`);
    if (err.message.includes("404")) {
      console.error(`\n    The release may not exist for your platform yet.`);
      console.error(`    Check: https://atomgit.com/atomgit_atomcode/atomcode/releases`);
    }
    process.exit(1);
  }

  // Make executable (Unix)
  if (process.platform !== "win32") {
    fs.chmodSync(BIN_PATH, 0o755);
  }

  // Verify
  const stats = fs.statSync(BIN_PATH);
  const sizeKB = (stats.size / 1024).toFixed(0);
  console.log(`  ✓ Installed: ${BIN_PATH} (${sizeKB} KB)`);

  // Quick smoke test
  try {
    const { execFileSync } = require("child_process");
    const out = execFileSync(BIN_PATH, ["--version"], { encoding: "utf8" });
    console.log(`  ✓ ${out.trim()}`);
  } catch {
    // Non-fatal: --version might fail on musl-based systems etc.
  }

  console.log(`\n  🚀 Run 'atomcode' to start!\n`);
}

main().catch((err) => {
  console.error(`\n  ✗ Unexpected error: ${err.message}\n`);
  process.exit(1);
});
