import * as vscode from 'vscode';
import * as child_process from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import { DaemonClient } from './client';
import { HealthResponse } from './types';
import { DEFAULT_PORT } from '../config';

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

  constructor(client: DaemonClient, extensionUri: vscode.Uri, opts?: { defaultPort?: number; binaryPath?: string; autoStart?: boolean }) {
    this.client = client;
    this.extensionUri = extensionUri;
    this.defaultPort = opts?.defaultPort ?? DEFAULT_PORT;
    this.configBinaryPath = opts?.binaryPath ?? '';
    this.autoStart = opts?.autoStart ?? true;
  }

  async ensureRunning(): Promise<boolean> {
    const health = await this.tryGetHealth();

    if (health) {
      const expected = this.getExpectedVersion();
      if (!expected || health.version === expected) {
        // Version matches (or we can't determine expected version), reuse running daemon
        return true;
      }

      // Version mismatch — restart
      console.log(`[AtomCode] Daemon version mismatch: running=${health.version}, expected=${expected}. Restarting...`);

      const shutdownOk = await this.shutdownDaemon();
      if (shutdownOk) {
        console.log('[AtomCode] Old daemon stopped successfully');
      }

      const started = await this.start();
      if (!started) {
        // start() failed — check if another window already started the correct version
        const postHealth = await this.tryGetHealth();
        if (postHealth && postHealth.version === expected) {
          console.log(`[AtomCode] Daemon restarted to version ${expected}`);
          return true;
        }
        return false;
      }

      // Verify new version after start
      const newHealth = await this.tryGetHealth();
      if (newHealth && newHealth.version === expected) {
        console.log(`[AtomCode] Daemon restarted to version ${expected}`);
        return true;
      }

      // Another window may have started a different version, but daemon is running
      if (newHealth) {
        console.warn(`[AtomCode] New daemon version ${newHealth.version} does not match expected ${expected}`);
      }
      return true;
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

    // Step 2: Poll until daemon exits or timeout (3s for graceful)
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline) {
      await sleep(100);
      if (!(await this.client.isRunning())) {
        return true;
      }
    }

    // Step 3: Graceful shutdown failed — force kill the process on the port
    console.warn('[AtomCode] Graceful shutdown timed out, attempting force kill');
    try {
      if (process.platform === 'win32') {
        // Windows: find and kill process on port
        child_process.execSync(
          `for /f "tokens=5" %a in ('netstat -aon ^| findstr :${this.defaultPort} ^| findstr LISTENING') do taskkill /F /PID %a`,
          { stdio: 'ignore', shell: 'cmd.exe' }
        );
      } else {
        // macOS/Linux: lsof to find PID(s), then kill
        const output = child_process.execSync(
          `lsof -ti tcp:${this.defaultPort} -sTCP:LISTEN`,
          { encoding: 'utf-8' }
        ).trim();
        if (output) {
          for (const line of output.split('\n')) {
            const pid = parseInt(line.trim(), 10);
            if (pid > 0) {
              try { process.kill(pid, 'SIGTERM'); } catch { /* already exited */ }
            }
          }
        }
      }
    } catch {
      // kill failed — process may have already exited or we lack permissions
    }

    // Step 4: Wait briefly for SIGTERM to take effect
    const forceDeadline = Date.now() + 2000;
    while (Date.now() < forceDeadline) {
      await sleep(100);
      if (!(await this.client.isRunning())) {
        return true;
      }
    }

    // Step 5: SIGTERM didn't work — escalate to SIGKILL (Unix only)
    if (process.platform !== 'win32') {
      console.warn('[AtomCode] SIGTERM failed, sending SIGKILL');
      try {
        const output = child_process.execSync(
          `lsof -ti tcp:${this.defaultPort} -sTCP:LISTEN`,
          { encoding: 'utf-8' }
        ).trim();
        if (output) {
          for (const line of output.split('\n')) {
            const pid = parseInt(line.trim(), 10);
            if (pid > 0) {
              try { process.kill(pid, 'SIGKILL'); } catch { /* already exited */ }
            }
          }
        }
      } catch { /* ignore */ }

      // Brief wait for SIGKILL to take effect
      await sleep(500);
      if (!(await this.client.isRunning())) {
        return true;
      }
    }

    console.warn('[AtomCode] Daemon did not exit after force kill. Manual intervention may be needed.');
    return false;
  }

  private async start(): Promise<boolean> {
    const port = this.defaultPort;
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
      const resolved = child_process.execSync(command, { encoding: 'utf-8' }).trim();
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
      // Developer build outputs — these paths are expected to fail in production
      path.join(workspaceRoot, 'target', 'release', 'atomcode-daemon'),
      path.join(workspaceRoot, 'target', 'debug', 'atomcode-daemon'),
    ];
    for (const p of daemonPaths) {
      if (fs.existsSync(p)) {
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
    // Don't kill daemon on extension deactivate — it may be shared with other windows
  }
}
