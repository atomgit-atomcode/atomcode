import { ChildProcess, spawn } from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as net from 'net';

/**
 * Manages the atomcode-daemon subprocess lifecycle.
 *
 * In development, it looks for the binary at ../target/release/atomcode-daemon
 * (or debug). In production, it finds the bundled binary in app resources.
 */
export class DaemonManager {
  private proc: ChildProcess | null = null;
  private port: number = 13456;
  private running = false;

  /**
   * Resolve the path to the atomcode-daemon binary.
   */
  private findBinary(): string {
    const isWin = process.platform === 'win32';
    const exeName = `atomcode-daemon${isWin ? '.exe' : ''}`;

    // In development: look in the Rust build output
    const devPaths = [
      path.join(__dirname, '../../target/release', exeName),
      path.join(__dirname, '../../target/debug', exeName),
    ];

    // In production (packaged): bundled in extraResources
    const resourcesPath = (process as any).resourcesPath || '';
    const prodPath = path.join(resourcesPath, 'daemon', exeName);

    // Check production first (if running packaged), then dev paths
    if (fs.existsSync(prodPath)) {
      return prodPath;
    }

    for (const p of devPaths) {
      if (fs.existsSync(p)) {
        return p;
      }
    }

    // Return the release path as default (will fail with helpful error)
    return devPaths[0];
  }

  /**
   * Find a free port by asking the OS.
   */
  private async findFreePort(preferred: number): Promise<number> {
    return new Promise((resolve, reject) => {
      const server = net.createServer();
      server.listen(preferred, '127.0.0.1', () => {
        const addr = server.address();
        server.close(() => {
          if (addr && typeof addr === 'object') {
            resolve(addr.port);
          } else {
            resolve(preferred);
          }
        });
      });
      server.on('error', () => {
        // Preferred port taken, try 0 (OS-assigned)
        const fallback = net.createServer();
        fallback.listen(0, '127.0.0.1', () => {
          const addr = fallback.address();
          fallback.close(() => {
            if (addr && typeof addr === 'object') {
              resolve(addr.port);
            } else {
              reject(new Error('Could not find free port'));
            }
          });
        });
      });
    });
  }

  /**
   * Start the atomcode-daemon subprocess.
   */
  async start(): Promise<void> {
    if (this.running) return;

    const binPath = this.findBinary();

    if (!fs.existsSync(binPath)) {
      console.warn(`[daemon] Binary not found at: ${binPath}`);
      console.warn('[daemon] Please build the daemon first: cargo build -p atomcode-daemon --release');
      // Start anyway — renderer will show offline state
      this.running = false;
      return;
    }

    this.port = await this.findFreePort(13456);

    console.log(`[daemon] Starting: ${binPath} --port ${this.port}`);

    this.proc = spawn(binPath, ['--port', String(this.port), '--no-telemetry', '--idle-timeout', '0'], {
      stdio: ['ignore', 'pipe', 'pipe'],
      env: {
        ...process.env,
        ATOMCODE_DAEMON_ENABLE_DANGEROUS_TOOLS: '1',
      },
    });

    this.proc.stdout?.on('data', (data: Buffer) => {
      console.log(`[daemon:out] ${data.toString().trim()}`);
    });

    this.proc.stderr?.on('data', (data: Buffer) => {
      console.log(`[daemon:err] ${data.toString().trim()}`);
    });

    this.proc.on('exit', (code, signal) => {
      console.log(`[daemon] exited (code=${code}, signal=${signal})`);
      this.running = false;
      this.proc = null;
    });

    this.proc.on('error', (err) => {
      console.error(`[daemon] error: ${err.message}`);
      this.running = false;
    });

    // Wait for the daemon to be ready
    await this.waitForReady();
    this.running = true;
  }

  /**
   * Wait for the daemon HTTP server to accept connections.
   */
  private async waitForReady(timeoutMs = 15000): Promise<void> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      try {
        await new Promise<void>((resolve, reject) => {
          const client = new net.Socket();
          client.connect(this.port, '127.0.0.1', () => {
            client.destroy();
            resolve();
          });
          client.on('error', (err) => {
            client.destroy();
            reject(err);
          });
        });
        return; // Connected!
      } catch {
        // Not ready yet, wait a bit
        await new Promise((r) => setTimeout(r, 200));
      }
    }
    throw new Error(`Daemon did not start within ${timeoutMs}ms`);
  }

  /**
   * Stop the daemon process.
   */
  async stop(): Promise<void> {
    if (!this.proc) return;

    return new Promise((resolve) => {
      const timeout = setTimeout(() => {
        // Force kill if graceful shutdown doesn't work
        if (this.proc) {
          this.proc.kill('SIGKILL');
          this.proc = null;
        }
        this.running = false;
        resolve();
      }, 5000);

      if (this.proc) {
        this.proc.on('exit', () => {
          clearTimeout(timeout);
          this.running = false;
          this.proc = null;
          resolve();
        });
        this.proc.kill('SIGTERM');
      } else {
        clearTimeout(timeout);
        this.running = false;
        resolve();
      }
    });
  }

  getPort(): number {
    return this.port;
  }

  isRunning(): boolean {
    return this.running;
  }
}
