#!/usr/bin/env node
/**
 * AtomCode npm uninstaller — postuninstall hook
 *
 * Cleans up the downloaded binary and bin/ directory.
 */
"use strict";

const fs = require("fs");
const path = require("path");

const BIN_DIR = path.join(__dirname, "bin");

function rmSync(dir) {
  if (!fs.existsSync(dir)) return;
  for (const entry of fs.readdirSync(dir)) {
    const p = path.join(dir, entry);
    const stat = fs.lstatSync(p);
    if (stat.isDirectory()) {
      rmSync(p);
    } else {
      fs.unlinkSync(p);
    }
  }
  fs.rmdirSync(dir);
}

if (fs.existsSync(BIN_DIR)) {
  rmSync(BIN_DIR);
  console.log("  ✓ AtomCode binary removed.");
}
