#!/usr/bin/env node
/**
 * AtomCode CLI wrapper — bin entry point for the npm package.
 *
 * Locates the platform-specific binary downloaded during postinstall
 * and exec's it with the same arguments, preserving stdin/stdout/stderr.
 */
"use strict";

const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const IS_WIN32 = process.platform === "win32";
const BIN_NAME = `atomcode${IS_WIN32 ? ".exe" : ""}`;

// Resolve binary relative to this script's location
const BIN_PATH = path.join(__dirname, "bin", BIN_NAME);

if (!fs.existsSync(BIN_PATH)) {
  console.error(
    `\n  ⚠ Binary not found at: ${BIN_PATH}\n` +
      `\n  The AtomCode binary was not downloaded. This can happen when:\n` +
      `    • npm's --ignore-scripts flag was used\n` +
      `    • The postinstall script was interrupted\n` +
      `  \n` +
      `  To fix:\n` +
      `    npm rebuild @atomgit/atomcode\n` +
      `  \n` +
      `  Or install via the shell script:\n` +
      `    curl -fsSL https://atomgit.com/atomgit_atomcode/atomcode/raw/main/install.sh | sh\n`
  );
  process.exit(1);
}

try {
  execFileSync(BIN_PATH, process.argv.slice(2), {
    stdio: "inherit",
    env: process.env,
  });
} catch (err) {
  // Propagate the binary's exit code
  if (err.status != null) {
    process.exit(err.status);
  }
  process.exit(1);
}
