import * as vscode from 'vscode';
import * as child_process from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import { DaemonClient } from './client';

export class DaemonProcess {
  private process?: child_process.ChildProcess;
  private client: DaemonClient;

  constructor(client: DaemonClient) {
    this.client = client;
  }

  async ensureRunning(): Promise<boolean> {
    if (await this.client.isRunning()) {
      return true;
    }

    const config = vscode.workspace.getConfiguration('atomcode');
    if (!config.get<boolean>('daemon.autoStart', true)) {
      return false;
    }

    return this.start();
  }

  private async start(): Promise<boolean> {
    const config = vscode.workspace.getConfiguration('atomcode');
    const port = config.get<number>('daemon.port', 13456);
    const binary = this.findBinary(port);
    if (!binary) {
      vscode.window.showErrorMessage(
        'AtomCode not found. Install atomcode (cargo install atomcode) or set atomcode.daemon.binaryPath in settings.'
      );
      return false;
    }

    // `atomcode daemon` starts the HTTP daemon as a subcommand.
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
   * 2. `atomcode` in PATH (uses `atomcode daemon` subcommand)
   * 3. Common install locations
   * 4. Workspace build outputs (for developers)
   */
  private findBinary(port: number): { path: string; args: string[] } | undefined {
    const config = vscode.workspace.getConfiguration('atomcode');
    const configured = config.get<string>('daemon.binaryPath', '');
    const portArgs = ['--port', String(port)];

    // 1. User-configured path (could be atomcode or atomcode-daemon)
    if (configured && fs.existsSync(configured)) {
      const name = path.basename(configured);
      if (name.includes('daemon')) {
        return { path: configured, args: portArgs };
      }
      return { path: configured, args: ['daemon', ...portArgs] };
    }

    // 2. Check PATH via `which`
    try {
      const resolved = child_process.execSync('which atomcode 2>/dev/null', { encoding: 'utf-8' }).trim();
      if (resolved) {
        return { path: resolved, args: ['daemon', ...portArgs] };
      }
    } catch {
      // not in PATH
    }

    const home: string = os.homedir();
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath || '';

    // 3. Common install locations (atomcode main binary with daemon subcommand)
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

    // 4. Standalone atomcode-daemon binary (fallback)
    const daemonPaths = [
      path.join(home, '.atomcode', 'bin', 'atomcode-daemon'),
      path.join(home, '.cargo', 'bin', 'atomcode-daemon'),
      '/usr/local/bin/atomcode-daemon',
      // Developer build outputs
      path.join(workspaceRoot, 'target', 'release', 'atomcode-daemon'),
      path.join(workspaceRoot, 'target', 'debug', 'atomcode-daemon'),
      path.join(home, 'Desktop', 'atomcode', 'target', 'release', 'atomcode-daemon'),
    ];
    for (const p of daemonPaths) {
      if (fs.existsSync(p)) {
        return { path: p, args: portArgs };
      }
    }

    return undefined;
  }

  dispose(): void {
    // Don't kill daemon on extension deactivate — it may be shared with other windows
  }
}
