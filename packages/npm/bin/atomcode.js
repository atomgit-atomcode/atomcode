#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const { existsSync } = require("fs");
const path = require("path");

const PLATFORM_PACKAGES = {
  darwin: {
    arm64: "@atomgit.com/atomcode-darwin-arm64",
    x64: "@atomgit.com/atomcode-darwin-x64",
  },
  linux: {
    arm64: "@atomgit.com/atomcode-linux-arm64",
    x64: "@atomgit.com/atomcode-linux-x64",
  },
  win32: {
    x64: "@atomgit.com/atomcode-win32-x64",
  },
  ohos: {
    arm64: "@atomgit.com/atomcode-ohos-arm64",
  },
};

const os = process.platform;
const arch = process.arch;
const pkgName = PLATFORM_PACKAGES[os]?.[arch];

if (!pkgName) {
  console.error(
    `Unsupported platform: ${os} ${arch}. ` +
      "AtomCode provides binaries for darwin (arm64/x64), linux (arm64/x64), windows (x64), and ohos (arm64)."
  );
  process.exit(1);
}

let pkgDir;
try {
  pkgDir = path.dirname(require.resolve(`${pkgName}/package.json`));
} catch {
  console.error(
    `Binary package ${pkgName} not found.\n` +
      "  This can happen when --ignore-scripts was used or the install was interrupted.\n" +
      "  To fix: npm rebuild @atomgit.com/atomcode\n" +
      "  Or reinstall: npm install -g @atomgit.com/atomcode\n"
  );
  process.exit(1);
}

const binName = os === "win32" ? "atomcode.exe" : "atomcode";
const binPath = path.join(pkgDir, "bin", binName);

if (!existsSync(binPath)) {
  console.error(
    `Binary not found at ${binPath}.\n  Reinstall: npm install -g @atomgit.com/atomcode\n`
  );
  process.exit(1);
}

const child = spawn(binPath, process.argv.slice(2), {
  stdio: "inherit",
  env: process.env,
});

["SIGINT", "SIGTERM", "SIGHUP"].forEach((sig) => {
  process.on(sig, () => {
    if (!child.killed) child.kill(sig);
  });
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
