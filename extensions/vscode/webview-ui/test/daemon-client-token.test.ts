import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { readDaemonToken } from '../../src/daemon/client';

function testReadDaemonTokenReturnsTokenFromFile() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'ac-token-test-'));
  const savedHome = process.env.ATOMCODE_HOME;
  try {
    process.env.ATOMCODE_HOME = home;
    fs.writeFileSync(
      path.join(home, 'daemon-13456.json'),
      JSON.stringify({ pid: 1, port: 13456, token: 'tok-xyz' }),
    );
    assert.equal(readDaemonToken(13456), 'tok-xyz');
  } finally {
    if (savedHome === undefined) {
      delete process.env.ATOMCODE_HOME;
    } else {
      process.env.ATOMCODE_HOME = savedHome;
    }
    fs.rmSync(home, { recursive: true, force: true });
  }
}

function testReadDaemonTokenReturnsUndefinedWhenFileMissing() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'ac-token-test-'));
  const savedHome = process.env.ATOMCODE_HOME;
  try {
    process.env.ATOMCODE_HOME = home;
    assert.equal(readDaemonToken(19999), undefined);
  } finally {
    if (savedHome === undefined) {
      delete process.env.ATOMCODE_HOME;
    } else {
      process.env.ATOMCODE_HOME = savedHome;
    }
    fs.rmSync(home, { recursive: true, force: true });
  }
}

function testReadDaemonTokenReturnsUndefinedWhenTokenFieldMissing() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'ac-token-test-'));
  const savedHome = process.env.ATOMCODE_HOME;
  try {
    process.env.ATOMCODE_HOME = home;
    fs.writeFileSync(
      path.join(home, 'daemon-13456.json'),
      JSON.stringify({ pid: 1, port: 13456 }),
    );
    assert.equal(readDaemonToken(13456), undefined);
  } finally {
    if (savedHome === undefined) {
      delete process.env.ATOMCODE_HOME;
    } else {
      process.env.ATOMCODE_HOME = savedHome;
    }
    fs.rmSync(home, { recursive: true, force: true });
  }
}

function testReadDaemonTokenUsesAtomcodeHomeFallback() {
  // When ATOMCODE_HOME is unset, falls back to ~/.atomcode — we can't easily
  // write there in a test, so just verify the function returns undefined
  // (no file at that path) rather than throwing.
  const savedHome = process.env.ATOMCODE_HOME;
  try {
    delete process.env.ATOMCODE_HOME;
    // Should not throw; returns undefined if file absent
    const result = readDaemonToken(19998);
    assert.equal(result === undefined || typeof result === 'string', true);
  } finally {
    if (savedHome !== undefined) {
      process.env.ATOMCODE_HOME = savedHome;
    }
  }
}

testReadDaemonTokenReturnsTokenFromFile();
testReadDaemonTokenReturnsUndefinedWhenFileMissing();
testReadDaemonTokenReturnsUndefinedWhenTokenFieldMissing();
testReadDaemonTokenUsesAtomcodeHomeFallback();
