#!/usr/bin/env node
const fs = require('fs');
const path = require('path');

const root = path.resolve(__dirname, '..');
const repoRoot = path.resolve(root, '..', '..');
const outRoot = path.join(root, 'resources', 'bin');
const requireBinary = process.argv.includes('--require');

const targets = [
  { id: 'darwin-arm64', exe: 'atomcode-daemon', env: 'ATOMCODE_DAEMON_DARWIN_ARM64' },
  { id: 'darwin-x64', exe: 'atomcode-daemon', env: 'ATOMCODE_DAEMON_DARWIN_X64' },
  { id: 'linux-x64', exe: 'atomcode-daemon', env: 'ATOMCODE_DAEMON_LINUX_X64' },
  { id: 'linux-arm64', exe: 'atomcode-daemon', env: 'ATOMCODE_DAEMON_LINUX_ARM64' },
  { id: 'win32-x64', exe: 'atomcode-daemon.exe', env: 'ATOMCODE_DAEMON_WIN32_X64' },
];

function currentTargetId() {
  if (process.platform === 'darwin' && process.arch === 'arm64') return 'darwin-arm64';
  if (process.platform === 'darwin' && process.arch === 'x64') return 'darwin-x64';
  if (process.platform === 'linux' && process.arch === 'x64') return 'linux-x64';
  if (process.platform === 'linux' && process.arch === 'arm64') return 'linux-arm64';
  if (process.platform === 'win32' && process.arch === 'x64') return 'win32-x64';
  return undefined;
}

// Map platform IDs to Rust target triples for cross-compilation lookup.
function targetTriple(platformId) {
  const triples = {
    'darwin-arm64': 'aarch64-apple-darwin',
    'darwin-x64': 'x86_64-apple-darwin',
    'linux-x64': 'x86_64-unknown-linux-gnu',
    'linux-arm64': 'aarch64-unknown-linux-gnu',
    'win32-x64': 'x86_64-pc-windows-msvc',
  };
  return triples[platformId];
}

function sourceFor(target) {
  const explicit = process.env[target.env];
  if (explicit) return path.resolve(explicit);

  if (target.id === currentTargetId()) {
    const localExe = process.platform === 'win32' ? 'atomcode-daemon.exe' : 'atomcode-daemon';
    // Search native (no --target) and cross-compilation (--target <triple>) build outputs.
    const triple = targetTriple(target.id);
    for (const profile of ['release', 'debug']) {
      const candidates = [path.join(repoRoot, 'target', profile, localExe)];
      if (triple) {
        candidates.push(path.join(repoRoot, 'target', triple, profile, localExe));
      }
      for (const c of candidates) {
        if (fs.existsSync(c)) return c;
      }
    }
  }

  return undefined;
}

let copied = 0;
const missing = [];

for (const target of targets) {
  const source = sourceFor(target);
  if (!source || !fs.existsSync(source)) {
    missing.push(`${target.id} (${target.env})`);
    continue;
  }

  const destinationDir = path.join(outRoot, target.id);
  const destination = path.join(destinationDir, target.exe);
  fs.mkdirSync(destinationDir, { recursive: true });
  fs.copyFileSync(source, destination);
  if (target.id !== 'win32-x64') {
    fs.chmodSync(destination, 0o755);
  }
  copied += 1;
  console.log(`[bundle-daemon] ${target.id}: ${source} -> ${destination}`);
}

if (missing.length > 0) {
  const message = `[bundle-daemon] missing binaries: ${missing.join(', ')}`;
  if (requireBinary) {
    console.error(message);
    console.error('[bundle-daemon] set the listed env vars before packaging for the marketplace.');
    process.exit(1);
  }
  console.warn(message);
}

if (copied === 0 && requireBinary) {
  console.error('[bundle-daemon] no daemon binaries were bundled.');
  process.exit(1);
}

// Write daemon-version.txt from workspace Cargo.toml so the extension can
// compare the bundled daemon version against the running daemon's /health.
const cargoToml = path.join(repoRoot, 'Cargo.toml');
if (fs.existsSync(cargoToml)) {
  const content = fs.readFileSync(cargoToml, 'utf-8');
  const match = content.match(/^\[workspace\.package\]\s*\n(?:.*\n)*?version\s*=\s*"([^"]+)"/m);
  if (match) {
    const versionFile = path.join(outRoot, 'daemon-version.txt');
    fs.writeFileSync(versionFile, match[1].trim());
    console.log(`[bundle-daemon] wrote daemon-version.txt: ${match[1].trim()}`);
  } else {
    console.warn('[bundle-daemon] could not parse version from workspace Cargo.toml');
  }
}
