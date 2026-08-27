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
import type { ExpectedDaemonIdentity } from './identity';

const sleep = (ms: number) => new Promise(r => setTimeout(r, ms));

interface DaemonBinary {
  path: string;
  args: string[];
}

export class DaemonProcess {
  private process?: child_process.ChildProcess;
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
    const health = await this.tryGetHealth();

    if (health) {
      const expected = this.getExpectedBundledIdentity();
      if (daemonIdentityMatches(health, expected)) {
        // Exact bundled build matches (or no bundled identity applies).
        return true;
      }

      // Same-version daemon builds can expose different config/runtime schemas.
      // Compare the executable hash as well so an older 5.0.9 process cannot
      // survive an extension update that bundles a newer 5.0.9 daemon.
      console.log(
        `[AtomCode] Daemon identity mismatch: running=${health.version}/${health.binary_hash || 'unknown'}, `
        + `expected=${expected.version || 'any'}/${expected.binaryHash || 'any'}. Restarting...`,
      );

      const shutdownOk = await this.shutdownDaemon();
      if (shutdownOk) {
        console.log('[AtomCode] Old daemon stopped successfully');
      } else {
        console.warn(
          `[AtomCode] Refusing to start daemon because an incompatible daemon ${health.version} is still running`
        );
        vscode.window.showWarningMessage(
          'AtomCode daemon build mismatch. AtomCode could not stop the old daemon. Please stop the old AtomCode daemon or reload VS Code.'
        );
        return false;
      }

      const started = await this.start();
      if (!started) {
        // start() failed — check if another window already started the correct version
        const postHealth = await this.tryGetHealth();
        if (postHealth && daemonIdentityMatches(postHealth, expected)) {
          console.log(`[AtomCode] Daemon restarted to version ${postHealth.version}`);
          return true;
        }
        return false;
      }

      // Verify new version after start
      const newHealth = await this.tryGetHealth();
      if (newHealth && daemonIdentityMatches(newHealth, expected)) {
        console.log(`[AtomCode] Daemon restarted to version ${newHealth.version}`);
        return true;
      }

      // Another window may have started a different version, but daemon is running
      if (newHealth) {
        console.warn(`[AtomCode] New daemon version ${newHealth.version} does not match expected ${expected}`);
      }
      return false;
    }

    // Daemon not running
    if (!this.autoStart) {
      return false;
    }

    return this.start();
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

  private async tryGetHealth(): Promise<HealthResponse | null> {
    try {
      return await this.client.health();
    } catch {
      return null;
    }
  }

  private async shutdownDaemon(): Promise<boolean> {
    // Step 1: Try graceful shutdown via HTTP endpoint
    try {
      await this.client.shutdown();
    } catch {
      // Old daemon may not support /shutdown, or already exiting — ignore
    }

    // Step 2: Poll until daemon exits or timeout (5s for graceful)
    // We only wait for the daemon we own (tracked by this.process) or the
    // HTTP endpoint to stop responding. We do NOT kill arbitrary processes
    // on the port — that would be unsafe for other users' services.
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline) {
      await sleep(100);
      if (!(await this.client.isRunning())) {
        return true;
      }
    }

    // Step 3: If we spawned the daemon ourselves, send SIGTERM to our own child.
    // This is safe because we own the process reference.
    if (this.process && !this.process.killed) {
      console.warn('[AtomCode] Graceful shutdown timed out, sending SIGTERM to owned daemon process');
      try {
        this.process.kill('SIGTERM');
      } catch {
        // Process may have already exited
      }

      // Wait for our child process to exit
      const termDeadline = Date.now() + 2000;
      while (Date.now() < termDeadline) {
        await sleep(100);
        if (!(await this.client.isRunning())) {
          return true;
        }
      }

      // Last resort: SIGKILL our own child process only
      if (!this.process.killed) {
        console.warn('[AtomCode] SIGTERM failed, sending SIGKILL to owned daemon process');
        try {
          this.process.kill('SIGKILL');
        } catch { /* already exited */ }
        await sleep(300);
      }
    }

    if (!(await this.client.isRunning())) {
      return true;
    }

    console.warn('[AtomCode] Daemon did not exit. It may have been started by another process.');
    return false;
  }

  private async start(): Promise<boolean> {
    const port = this.defaultPort;
    // Recovery: if a PREVIOUS daemon wedged and is squatting the port (health fails but
    // the process is alive), reap it before spawning so our fresh daemon can bind. Such
    // a daemon isn't our owned child, so shutdownDaemon() can't SIGKILL it — leaving the
    // port stuck ("started but not responding") across restarts and even reinstalls.
    await this.reapWedgedDaemon(port);
    const binary = this.findBinary(port);
    if (!binary) {
      vscode.window.showErrorMessage(
        'AtomCode daemon not found for this platform. Reinstall the AtomCode extension, install atomcode, or set atomcode.daemon.binaryPath in settings.'
      );
      return false;
    }

    // Use the first workspace folder as cwd so the daemon can find project-level
    // .mcp.json and detect repo_origin correctly.
    const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

    this.process = child_process.spawn(binary.path, binary.args, {
      detached: true,
      stdio: 'ignore',
      windowsHide: true,
      env: normalizeDaemonEnvForUtf8Locale(process.env),
      ...(cwd ? { cwd } : {}),
    });
    this.process.unref();

    // Wait up to 10s for daemon to be ready
    for (let i = 0; i < 100; i++) {
      await new Promise((r) => setTimeout(r, 100));
      if (await this.client.isRunning()) {
        return true;
      }
    }

    vscode.window.showWarningMessage(
      `AtomCode daemon started but not responding. Check if port ${port} is available.`
    );
    return false;
  }

  /** `$ATOMCODE_HOME` or `~/.atomcode` — mirrors the daemon's `Config::config_dir()`. */
  private atomcodeHome(): string {
    const env = process.env.ATOMCODE_HOME;
    return env && env.length > 0 ? env : path.join(os.homedir(), '.atomcode');
  }

  /** Pidfile the daemon writes: `<home>/daemon-<port>.json`, keyed by port. */
  private pidfilePath(port: number): string {
    return path.join(this.atomcodeHome(), `daemon-${port}.json`);
  }

  /**
   * Reap a WEDGED daemon that squats `port` but no longer answers /health. It may have
   * been started by a previous VS Code session (not our owned child), so shutdownDaemon()
   * can't kill it — the recovery gap that made the port stay stuck through restarts and
   * reinstalls. Uses the daemon's own pidfile to identify the target and force-kills it
   * ONLY after validating (a) health is genuinely dead now — a transient blip must not
   * kill a daemon other windows are using, and (b) the pid is live AND an atomcode image
   * — so a reused pid is never mis-killed. Best-effort; any failure falls through to a
   * normal spawn (no worse than today).
   */
  private async reapWedgedDaemon(port: number): Promise<void> {
    // (a) Only reap if the daemon is genuinely unresponsive. Retry a few times so a
    // momentary hiccup doesn't kill a healthy daemon.
    for (let i = 0; i < 3; i++) {
      if (await this.client.isRunning()) {
        return; // responded → healthy, do not touch
      }
      await sleep(200);
    }

    let pid: number | undefined;
    try {
      const info = JSON.parse(fs.readFileSync(this.pidfilePath(port), 'utf-8')) as { pid?: number };
      pid = info.pid;
    } catch {
      return; // no/garbage pidfile → nothing to reap
    }
    if (typeof pid !== 'number' || !Number.isInteger(pid) || pid <= 0 || pid === process.pid) {
      return;
    }

    // (b) Is the process alive? kill(pid, 0) throws (ESRCH) when it isn't.
    try {
      process.kill(pid, 0);
    } catch {
      return; // dead pid → stale pidfile, ignore
    }
    // (b) Validate it's actually an atomcode daemon before killing (PID-reuse guard).
    if (!this.isAtomcodeProcess(pid)) {
      return;
    }

    console.warn(`[AtomCode] Reaping wedged daemon pid=${pid} squatting port ${port}`);
    try {
      if (process.platform === 'win32') {
        // /F force, /T also terminate any child tree.
        child_process.execFileSync('taskkill', ['/F', '/T', '/PID', String(pid)], { stdio: 'ignore', windowsHide: true });
      } else {
        process.kill(pid, 'SIGKILL');
      }
    } catch {
      // Best effort — if the kill fails, the spawn below will still surface the error.
    }

    // Wait for the OS to reap the process (and release the port) — up to ~5s, since a
    // force-kill + socket teardown can lag on a loaded Windows box. If it outlasts this,
    // the spawn below fails and the user's next attempt reaps again (pid now gone).
    for (let i = 0; i < 50; i++) {
      await sleep(100);
      try {
        process.kill(pid, 0);
      } catch {
        return; // pid gone → port should now be free
      }
    }
  }

  /** Best-effort check that `pid`'s image is an atomcode daemon (guards PID reuse). */
  private isAtomcodeProcess(pid: number): boolean {
    try {
      if (process.platform === 'win32') {
        const out = child_process.execFileSync(
          'tasklist',
          ['/FI', `PID eq ${pid}`, '/FO', 'CSV', '/NH'],
          { encoding: 'utf-8', windowsHide: true }
        );
        return /atomcode/i.test(out);
      }
      const out = child_process.execFileSync('ps', ['-p', String(pid), '-o', 'comm='], {
        encoding: 'utf-8',
      });
      return /atomcode/i.test(out);
    } catch {
      return false; // can't verify → don't kill (safe default)
    }
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
    this.process = undefined;
  }
}
