import * as http from 'http';
import {
  ChatRequest,
  ChatStreamCallbacks,
  ChatEvent,
  HealthResponse,
  ModelInfo,
  ProjectState,
  SessionMeta,
  SessionDetail,
  CreateSessionResponse,
  ChangeDirResponse,
} from './types';

const REST_TIMEOUT = 5000;

export class DaemonClient {
  private baseUrl: string;
  private host: string;
  private port: number;

  constructor(port: number) {
    this.port = port;
    this.host = '127.0.0.1';
    this.baseUrl = `http://${this.host}:${this.port}`;
  }

  // ── REST helpers ──────────────────────────────────────────────

  private request<T>(method: string, path: string, body?: unknown): Promise<T> {
    return new Promise((resolve, reject) => {
      const payload = body ? JSON.stringify(body) : undefined;
      const options: http.RequestOptions = {
        hostname: this.host,
        port: this.port,
        path,
        method,
        headers: {
          'Content-Type': 'application/json',
          ...(payload ? { 'Content-Length': Buffer.byteLength(payload) } : {}),
        },
        timeout: REST_TIMEOUT,
      };

      const req = http.request(options, (res) => {
        const chunks: Buffer[] = [];
        res.on('data', (chunk: Buffer) => chunks.push(chunk));
        res.on('end', () => {
          const raw = Buffer.concat(chunks).toString('utf-8');
          if (!res.statusCode || res.statusCode >= 400) {
            reject(new Error(`HTTP ${res.statusCode}: ${raw}`));
            return;
          }
          try {
            resolve(JSON.parse(raw) as T);
          } catch {
            reject(new Error(`Invalid JSON response: ${raw}`));
          }
        });
      });

      req.on('timeout', () => {
        req.destroy();
        reject(new Error('Request timed out'));
      });

      req.on('error', (err: NodeJS.ErrnoException) => {
        if (err.code === 'ECONNREFUSED') {
          reject(new Error('Daemon not running'));
        } else {
          reject(err);
        }
      });

      if (payload) {
        req.write(payload);
      }
      req.end();
    });
  }

  private get<T>(path: string): Promise<T> {
    return this.request<T>('GET', path);
  }

  private post<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>('POST', path, body);
  }

  private patch<T>(path: string, body?: unknown): Promise<T> {
    return this.request<T>('PATCH', path, body);
  }

  private delete<T>(path: string): Promise<T> {
    return this.request<T>('DELETE', path);
  }

  // ── Health ────────────────────────────────────────────────────

  async isRunning(): Promise<boolean> {
    try {
      await this.health();
      return true;
    } catch {
      return false;
    }
  }

  health(): Promise<HealthResponse> {
    return this.get<HealthResponse>('/health');
  }

  // ── Project ───────────────────────────────────────────────────

  getProject(): Promise<ProjectState> {
    return this.get<ProjectState>('/project');
  }

  changeDir(dir: string): Promise<ChangeDirResponse> {
    return this.post<ChangeDirResponse>('/project/cd', { path: dir });
  }

  // ── Models ────────────────────────────────────────────────────

  listModels(): Promise<ModelInfo[]> {
    return this.get<ModelInfo[]>('/models');
  }

  // ── Sessions ──────────────────────────────────────────────────

  listSessions(): Promise<SessionMeta[]> {
    return this.get<SessionMeta[]>('/sessions');
  }

  getSession(projectHash: string, id: string): Promise<SessionDetail> {
    return this.get<SessionDetail>(`/projects/${projectHash}/sessions/${id}`);
  }

  createSession(name?: string, workingDir?: string): Promise<CreateSessionResponse> {
    return this.post<CreateSessionResponse>('/sessions', {
      name,
      working_dir: workingDir,
    });
  }

  renameSession(id: string, name: string): Promise<SessionMeta> {
    return this.patch<SessionMeta>(`/sessions/${id}`, { name });
  }

  deleteSession(id: string): Promise<{ success: boolean }> {
    return this.delete<{ success: boolean }>(`/sessions/${id}`);
  }

  searchSessions(query: string): Promise<SessionMeta[]> {
    return this.get<SessionMeta[]>(`/sessions/search?q=${encodeURIComponent(query)}`);
  }

  // ── Chat (SSE) ────────────────────────────────────────────────

  streamChat(req: ChatRequest, callbacks: ChatStreamCallbacks): AbortController {
    const controller = new AbortController();
    const payload = JSON.stringify(req);

    const options: http.RequestOptions = {
      hostname: this.host,
      port: this.port,
      path: '/chat',
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Accept': 'text/event-stream',
        'Content-Length': Buffer.byteLength(payload),
      },
    };

    const httpReq = http.request(options, (res) => {
      if (!res.statusCode || res.statusCode >= 400) {
        const chunks: Buffer[] = [];
        res.on('data', (chunk: Buffer) => chunks.push(chunk));
        res.on('end', () => {
          const raw = Buffer.concat(chunks).toString('utf-8');
          callbacks.onError(`HTTP ${res.statusCode}: ${raw}`);
        });
        return;
      }

      let buffer = '';

      res.setEncoding('utf-8');
      res.on('data', (chunk: string) => {
        buffer += chunk;
        const lines = buffer.split('\n');
        // Keep the last potentially incomplete line in the buffer
        buffer = lines.pop() || '';

        for (const line of lines) {
          const trimmed = line.trim();

          // Skip empty lines and comments / keep-alive pings
          if (!trimmed || trimmed.startsWith(':')) {
            continue;
          }

          // SSE data line
          if (trimmed.startsWith('data: ')) {
            const data = trimmed.slice(6);
            this.handleSSEData(data, callbacks);
          }
        }
      });

      res.on('end', () => {
        // Process any remaining data in the buffer
        if (buffer.trim().startsWith('data: ')) {
          const data = buffer.trim().slice(6);
          this.handleSSEData(data, callbacks);
        }
      });

      res.on('error', (err) => {
        callbacks.onError(`Stream error: ${err.message}`);
      });
    });

    httpReq.on('error', (err: NodeJS.ErrnoException) => {
      if (err.code === 'ECONNREFUSED') {
        callbacks.onError('Daemon not running');
      } else if (controller.signal.aborted) {
        // Intentional abort, don't report as error
        callbacks.onStopped();
      } else {
        callbacks.onError(`Connection error: ${err.message}`);
      }
    });

    // Wire up abort
    controller.signal.addEventListener('abort', () => {
      httpReq.destroy();
    });

    httpReq.write(payload);
    httpReq.end();

    return controller;
  }

  private handleSSEData(data: string, callbacks: ChatStreamCallbacks): void {
    let event: ChatEvent;
    try {
      event = JSON.parse(data) as ChatEvent;
    } catch {
      // Skip malformed JSON, don't crash
      return;
    }

    switch (event.type) {
      case 'text':
        callbacks.onText(event.content);
        break;
      case 'tool_start':
        callbacks.onToolStart(event.name, event.arguments);
        break;
      case 'tool_result':
        callbacks.onToolResult(event.name, event.output, event.success, event.duration_ms);
        break;
      case 'tokens':
        callbacks.onTokens(event.prompt, event.completion, event.total);
        break;
      case 'artifact_start':
        callbacks.onArtifactStart(event.id, event.artifact_type, event.language, event.title);
        break;
      case 'artifact_content':
        callbacks.onArtifactContent(event.id, event.content);
        break;
      case 'artifact_end':
        callbacks.onArtifactEnd(event.id);
        break;
      case 'done':
        callbacks.onDone(event.tokens, event.tool_calls, event.session_id);
        break;
      case 'stopped':
        callbacks.onStopped();
        break;
      case 'error':
        callbacks.onError(event.message);
        break;
    }
  }

  // ── Stop generation ───────────────────────────────────────────

  stopGeneration(): Promise<{ success: boolean }> {
    return this.post<{ success: boolean }>('/chat/stop');
  }
}
