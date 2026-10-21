import * as vscode from 'vscode';
import * as child_process from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import { createHash } from 'crypto';
import { DaemonClient } from './client';
import { HealthResponse } from './types';
import { DEFAULT_PORT } from '../config';
import { normalizeDaemonEnvForUtf8Locale } from './env';
import { daemonIdentityMatches } from './identity';
import { acquireLaunchLock, daemonPorts, findCompatibleDaemon, isPortOccupied } from './connection';
import type { ExpectedDaemonIdentity } from './identity';

const sleep = (ms: number) => new Promise<void>(r => setTimeout(r, ms));

interface DaemonBinary {
  path: string;
  args: string[];
}

export class DaemonProcess {
  private disposed = false;
  private client: DaemonClient;
  private readonly extensionUri: vscode.Uri;
  private readonly defaultPort: number;
  private readonly configBinaryPath: string;
  private readonly autoStart: boolean;
  /** In-process lock: prevents concurrent ensureRunning() calls from spawning multiple daemons */
  private _ensureRunningPromise: Promise<boolean> | null = null;

  constructor(client: DaemonClient, extensionUri: vscode.Uri, opts?: { defaultPort?: number; binaryPath?: string; autoStart?: boolean }) {
    this.client = client;
    this.extensionUri = extensionUri;
    this.defaultPort = opts?.defaultPort ?? DEFAULT_PORT;
    this.configBinaryPath = opts?.binaryPath ?? '';
    this.autoStart = opts?.autoStart ?? true;
  }

  async ensureRunning(): Promise<boolean> {
    // Deduplicate concurrent calls within the same extension host process.
    // Multiple commands firing at startup would otherwise each try to spawn a daemon.
    if (this._ensureRunningPromise) {
      return this._ensureRunningPromise;
    }
    this._ensureRunningPromise = this._ensureRunningImpl().finally(() => {
      this._ensureRunningPromise = null;
    });
    return this._ensureRunningPromise;
  }

  private async _ensureRunningImpl(): Promise<boolean> {
    const previousPort = this.client.getPort();
    const expected = this.getExpectedBundledIdentity();
    const binary = this.findBinary(this.defaultPort);
    const identity = expected.binaryHash || expected.version || binary?.path || 'external';
    const ports = daemonPorts(this.defaultPort, this.client.getPort(), identity);
    const probes = new Map<number, DaemonClient>();
    const probe = (port: number) => {
      let client = probes.get(port);
      if (!client) { client = new DaemonClient(port); probes.set(port, client); }
      return client;
    };
    const port = await findCompatibleDaemon(ports, expected, this.autoStart, {
      health: async (candidate) => {
        try { return await probe(candidate).health(); } catch { return null; }
      },
      occupied: isPortOccupied,
      lock: acquireLaunchLock,
      start: (candidate) => this.start(candidate, expected, probe(candidate)),
      wait: () => sleep(100),
      active: () => !this.disposed,
    });
    if (port === null || this.disposed) {
      if (!this.disposed) vscode.window.showWarningMessage(vscode.l10n.t(
        'No compatible AtomCode daemon is available. Existing services were left running. Check the daemon binary or configure another daemon port.',
      ));
      return false;
    }
    this.client.setPort(port);
    if (port !== this.defaultPort) {
      if (port !== previousPort) vscode.window.setStatusBarMessage(vscode.l10n.t(
        'AtomCode: using port {0}; existing services on port {1} were left running.', port, this.defaultPort,
      ), 8000);
      console.log(`[AtomCode] Using compatible daemon on port ${port}; port ${this.defaultPort} was left untouched.`);
    }
    return true;
  }

  private getExpectedVersion(): string {
    // Read the daemon version from the bundled manifest file written by
    // bundle-daemon.js at package time. This is the CARGO_PKG_VERSION of
    // the daemon binary shipped with this extension — NOT the extension's
    // own package.json version (which uses a different versioning scheme).
    const versionFile = path.join(this.extensionUri.fsPath, 'resources', 'bin', 'daemon-version.txt');
    try {
      const version = fs.readFileSync(versionFile, 'utf-8').trim();
      if (version) {
        return version;
      }
    } catch {
      // File missing — extension may not have been packaged with bundle-daemon
    }
    console.warn('[AtomCode] Could not read daemon-version.txt, skipping version check');
    return '';
  }

  private getExpectedBundledIdentity(): ExpectedDaemonIdentity {
    // A user-configured/external daemon is authoritative. Only pin identity
    // when this extension will launch its own bundled executable.
    if (this.configBinaryPath) return {};
    const bundled = this.findBundledDaemon();
    if (!bundled) return {};

    let binaryHash: string | undefined;
    try {
      binaryHash = createHash('sha256').update(fs.readFileSync(bundled)).digest('hex');
    } catch {
      console.warn('[AtomCode] Could not hash bundled daemon, falling back to version check');
    }

    return {
      version: this.getExpectedVersion() || undefined,
      binaryHash,
    };
  }

  private async start(port: number, expected: ExpectedDaemonIdentity, probe: DaemonClient): Promise<boolean> {
    const binary = this.findBinary(port);
    if (!binary || this.disposed) return false;
    const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const child = child_process.spawn(binary.path, binary.args, {
      detached: true, stdio: 'ignore', windowsHide: true,
      env: normalizeDaemonEnvForUtf8Locale(process.env),
      ...(cwd ? { cwd } : {}),
    });
    let failed = false;
    child.once('error', () => { failed = true; });
    child.unref();
    const deadline = Date.now() + 10_000;
    for (let i = 0; i < 100 && !this.disposed && Date.now() < deadline; i++) {
      await sleep(100);
      let health: HealthResponse | null = null;
      try { health = await probe.health(); } catch { /* not ready */ }
      if (health && daemonIdentityMatches(health, expected)) return true;
      if (failed || child.exitCode !== null || child.signalCode !== null) return false;
      if (health) break; // An incompatible service won the bind race. Never stop it.
    }
    // Only the child handle created by THIS start attempt may be signalled.
    // Never POST /shutdown or infer process ownership from a pidfile/name.
    if (!this.disposed && child.exitCode === null && child.signalCode === null) {
      try { child.kill('SIGTERM'); } catch { /* already exited */ }
    }
    return false;
  }

  /**
   * Find the atomcode binary. Returns the path and args to start the daemon.
   *
   * Search order:
   * 1. User-configured binaryPath
   * 2. Bundled standalone atomcode-daemon in the extension package
   * 3. `atomcode` in PATH (uses `atomcode daemon` subcommand)
   * 4. Common install locations
   * 5. Workspace build outputs (for developers)
   */
  private findBinary(port: number): DaemonBinary | undefined {
    const portArgs = ['--port', String(port), '--client', 'vscode'];

    // 1. User-configured path (could be atomcode or atomcode-daemon)
    if (this.configBinaryPath && fs.existsSync(this.configBinaryPath)) {
      const name = path.basename(this.configBinaryPath);
      if (name.includes('daemon')) {
        return { path: this.configBinaryPath, args: portArgs };
      }
      return { path: this.configBinaryPath, args: ['daemon', ...portArgs] };
    }

    // 2. Bundled standalone atomcode-daemon binary
    const bundled = this.findBundledDaemon();
    if (bundled) {
      return { path: bundled, args: portArgs };
    }

    // 3. Check PATH via `which` (Unix) or `where` (Windows)
    try {
      const command = process.platform === 'win32' ? 'where atomcode' : 'which atomcode 2>/dev/null';
      const resolved = child_process.execSync(command, { encoding: 'utf-8', windowsHide: true }).trim();
      if (resolved) {
        // On Windows, 'where' returns all matches, take first line
        const firstMatch = process.platform === 'win32' ? resolved.split('\n')[0].trim() : resolved;
        return { path: firstMatch, args: ['daemon', ...portArgs] };
      }
    } catch {
      // not in PATH
    }

    const home: string = os.homedir();
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath || '';

    // 4. Common install locations (atomcode main binary with daemon subcommand)
    const atomcodePaths = [
      path.join(home, '.atomcode', 'bin', 'atomcode'),
      path.join(home, '.cargo', 'bin', 'atomcode'),
      '/usr/local/bin/atomcode',
    ];
    for (const p of atomcodePaths) {
      if (fs.existsSync(p)) {
        return { path: p, args: ['daemon', ...portArgs] };
      }
    }

    // 5. Standalone atomcode-daemon binary (fallback)
    const daemonPaths = [
      path.join(home, '.atomcode', 'bin', 'atomcode-daemon'),
      path.join(home, '.cargo', 'bin', 'atomcode-daemon'),
      '/usr/local/bin/atomcode-daemon',
    ];
    for (const p of daemonPaths) {
      if (fs.existsSync(p)) {
        return { path: p, args: portArgs };
      }
    }

    // Developer build outputs — only meaningful when running from source.
    // Warn the user so they know a dev build is being used instead of the
    // bundled daemon that should have shipped with the extension.
    const devPaths = [
      path.join(workspaceRoot, 'target', 'release', 'atomcode-daemon'),
      path.join(workspaceRoot, 'target', 'debug', 'atomcode-daemon'),
    ];
    for (const p of devPaths) {
      if (fs.existsSync(p)) {
        console.warn(`[AtomCode] Using dev build daemon: ${p}. The bundled daemon was not found — the extension package may be missing resources/bin/<platform>/atomcode-daemon.`);
        vscode.window.showWarningMessage(
          `AtomCode is using a development build of the daemon (${p}). The bundled daemon was not found. Reinstall the extension or set atomcode.daemon.binaryPath in settings.`
        );
        return { path: p, args: portArgs };
      }
    }

    return undefined;
  }

  private findBundledDaemon(): string | undefined {
    const platformDir = this.platformDir();
    if (!platformDir) {
      return undefined;
    }

    const executable = process.platform === 'win32' ? 'atomcode-daemon.exe' : 'atomcode-daemon';
    const bundled = path.join(this.extensionUri.fsPath, 'resources', 'bin', platformDir, executable);
    if (!fs.existsSync(bundled)) {
      return undefined;
    }

    this.ensureExecutable(bundled);
    return bundled;
  }

  private platformDir(): string | undefined {
    const platform = process.platform;
    const arch = process.arch;

    if (platform === 'darwin' && arch === 'arm64') return 'darwin-arm64';
    if (platform === 'darwin' && arch === 'x64') return 'darwin-x64';
    if (platform === 'linux' && arch === 'x64') return 'linux-x64';
    if (platform === 'linux' && arch === 'arm64') return 'linux-arm64';
    if (platform === 'win32' && arch === 'x64') return 'win32-x64';

    return undefined;
  }

  private ensureExecutable(filePath: string): void {
    if (process.platform === 'win32') {
      return;
    }

    try {
      fs.accessSync(filePath, fs.constants.X_OK);
    } catch {
      try {
        fs.chmodSync(filePath, 0o755);
      } catch {
        // If the extension install location is read-only, spawn will surface a useful error.
      }
    }
  }

  dispose(): void {
    // Don't kill daemon on extension deactivate — it may be shared with other windows.
    // We intentionally do NOT kill the process here; the daemon has its own idle
    // timeout and will exit on its own when no longer needed.
    this.disposed = true;
  }
}
