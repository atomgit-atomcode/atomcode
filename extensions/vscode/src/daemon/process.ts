import * as vscode from 'vscode';
import * as child_process from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import { DaemonClient } from './client';

export class DaemonProcess {
  private process?: child_process.ChildProcess;
  private client: DaemonClient;
  private port: number;

  constructor(client: DaemonClient, port: number) {
    this.client = client;
    this.port = port;
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
    const binary = this.findBinary();
    if (!binary) {
      vscode.window.showErrorMessage(
        'AtomCode binary not found. Install atomcode or set atomcode.daemon.binaryPath.'
      );
      return false;
    }

    // Spawn detached daemon
    this.process = child_process.spawn(binary, ['daemon', '--port', String(this.port)], {
      detached: true,
      stdio: 'ignore',
    });
    this.process.unref();

    // Wait up to 5s for daemon to be ready
    for (let i = 0; i < 50; i++) {
      await new Promise((r) => setTimeout(r, 100));
      if (await this.client.isRunning()) {
        return true;
      }
    }

    return false;
  }

  private findBinary(): string | undefined {
    const config = vscode.workspace.getConfiguration('atomcode');
    const configured = config.get<string>('daemon.binaryPath', '');
    if (configured && fs.existsSync(configured)) {
      return configured;
    }

    const home = process.env.HOME || '';

    // Search common paths
    const candidates = [
      'atomcode', // PATH
      path.join(home, '.atomcode', 'bin', 'atomcode'),
      '/usr/local/bin/atomcode',
      path.join(home, '.cargo', 'bin', 'atomcode'),
    ];

    // Try `which` for PATH-based lookup
    for (const c of candidates) {
      try {
        child_process.execSync(`which ${c} 2>/dev/null`);
        return c;
      } catch {
        // not found, continue
      }
    }

    // Try direct existence check for absolute paths
    for (const c of candidates.slice(1)) {
      if (fs.existsSync(c)) {
        return c;
      }
    }

    return undefined;
  }

  dispose(): void {
    // Don't kill daemon on extension deactivate — it may be shared
  }
}
