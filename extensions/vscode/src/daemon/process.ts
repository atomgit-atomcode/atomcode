import * as vscode from 'vscode';
import * as child_process from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import { DaemonClient } from './client';
import { DEFAULT_PORT } from '../config';

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
    if (await this.client.isRunning()) {
      return true;
    }

    if (!this.autoStart) {
      return false;
    }

    return this.start();
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

    this.process = child_process.spawn(binary.path, binary.args, {
      detached: true,
      stdio: 'ignore',
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
    const portArgs = ['--port', String(port)];

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
