import * as fs from 'fs';
import * as http from 'http';
import * as os from 'os';
import * as path from 'path';
import {
  ChatRequest,
  ChatStreamCallbacks,
  ApprovalMode,
  ApprovalModeResponse,
  AuthStatusResponse,
  ConfigResponse,
  ChatEvent,
  CodingPlanSetupResponse,
  CreateProviderRequest,
  HealthResponse,
  LoginPollResponse,
  LoginStartResponse,
  ModelInfo,
  PatchProviderRequest,
  PatchThinkingRequest,
  PermissionDecision,
  PermissionDecisionResponse,
  ProjectState,
  ProviderInfo,
  ProvidersResponse,
  SessionMeta,
  SessionDetail,
  CreateSessionResponse,
  ChangeDirResponse,
  SkillInfo,
  AppendSessionMessagesRequest,
  AppendSessionMessagesResponse,
} from './types';

/** `$ATOMCODE_HOME` or `~/.atomcode` — mirrors process.ts's `atomcodeHome()`. */
function atomcodeHome(): string {
  const env = process.env.ATOMCODE_HOME;
  return env && env.length > 0 ? env : path.join(os.homedir(), '.atomcode');
}

/**
 * Read the local daemon token from `<atomcodeHome()>/daemon-<port>.json`.
 * Returns the token string if present, or `undefined` on any error (file
 * absent, malformed JSON, empty/missing token field).
 */
export function readDaemonToken(port: number): string | undefined {
  try {
    const raw = fs.readFileSync(path.join(atomcodeHome(), `daemon-${port}.json`), 'utf-8');
    const info = JSON.parse(raw) as { token?: string };
    return info.token && info.token.length > 0 ? info.token : undefined;
  } catch {
    return undefined;
  }
}

const REST_TIMEOUT = 30000;

const REQUEST_BODY_TOO_LARGE_MESSAGE =
  '消息内容过大，发送失败。请压缩图片、减少附件数量，或缩短消息后重试。';

export function formatDaemonHttpError(statusCode: number | undefined, raw: string): string {
  let message = raw;
  try {
    const parsed = JSON.parse(raw) as { error?: string; message?: string };
    message = parsed.error || parsed.message || raw;
  } catch {
    // Keep the daemon's plain-text response.
  }

  if (
    statusCode === 413
    || message.includes('Failed to buffer the request body')
    || message.includes('length limit exceeded')
  ) {
    return REQUEST_BODY_TOO_LARGE_MESSAGE;
  }

  return message;
}

export function classifyDaemonStreamError(
  message: string,
  requestAborted: boolean,
): { type: 'stopped' } | { type: 'error'; message: string } {
  if (requestAborted || message === 'aborted') {
    return { type: 'stopped' };
  }
  return { type: 'error', message: `Stream error: ${message}` };
}

export class DaemonHttpError extends Error {
  constructor(
    public readonly statusCode: number | undefined,
    public readonly code: string | undefined,
    public readonly retryable: boolean,
    message: string,
  ) {
    super(message);
    this.name = 'DaemonHttpError';
  }
}

export class DaemonClient {
  private baseUrl: string;
  private host: string;
  private port: number;
  /** Called when an idempotent GET fails with ECONNREFUSED; successful restart permits one GET retry. */
  public onConnectionLost?: () => Promise<boolean>;
  /** Cached daemon token. `undefined` means "not read yet" — kept re-reading
   * until the daemon has written its file — and is invalidated on a 401 so a
   * restarted daemon's fresh token is picked up. Avoids a disk read per request. */
  private cachedToken: string | undefined;

  constructor(port: number) {
    this.port = port;
    this.host = '127.0.0.1';
    this.baseUrl = `http://${this.host}:${this.port}`;
  }

  /** Daemon token, read from disk once and cached. Re-reads while absent. */
  private token(): string | undefined {
    if (this.cachedToken === undefined) {
      this.cachedToken = readDaemonToken(this.port);
    }
    return this.cachedToken;
  }

  /** Drop the cached token so the next request re-reads it from disk. */
  private invalidateToken(): void {
    this.cachedToken = undefined;
  }

  // ── REST helpers ──────────────────────────────────────────────

  private request<T>(method: string, path: string, body?: unknown): Promise<T> {
    return this.requestOnce<T>(method, path, body).catch(async (err) => {
      // Stale token (daemon restarted → new token, or file not yet read): a 401
      // is rejected by the auth middleware BEFORE the handler runs, so replaying
      // is safe for any method. Invalidate the cache and retry once.
      if (err instanceof DaemonHttpError && err.statusCode === 401) {
        this.invalidateToken();
        return this.requestOnce<T>(method, path, body);
      }
      // Restart/retry only idempotent reads. Replaying POST/PATCH/DELETE after
      // process replacement can duplicate work or reuse daemon-owned stale IDs.
      if (method === 'GET'
          && err instanceof Error && err.message === 'Daemon not running'
          && this.onConnectionLost
          && path !== '/health' && path !== '/shutdown') {
        const restarted = await this.onConnectionLost();
        if (restarted) {
          return this.requestOnce<T>(method, path, body);
        }
      }
      throw err;
    });
  }

  private requestOnce<T>(method: string, path: string, body?: unknown): Promise<T> {
    return new Promise((resolve, reject) => {
      const payload = body ? JSON.stringify(body) : undefined;
      const token = this.token();
      const options: http.RequestOptions = {
        hostname: this.host,
        port: this.port,
        path,
        method,
        headers: {
          'Content-Type': 'application/json',
          'X-AtomCode-Client': 'vscode',
          ...(token ? { Authorization: `Bearer ${token}` } : {}),
          ...(payload ? { 'Content-Length': Buffer.byteLength(payload) } : {}),
        },
        timeout: REST_TIMEOUT,
      };

      const req = http.request(options, (res) => {
        const chunks: Buffer[] = [];
        res.on('data', (chunk: Buffer) => chunks.push(chunk));
        res.on('end', () => {
          const raw = Buffer.concat(chunks).toString('utf-8');
          const statusCode = res.statusCode;
          if (!statusCode || statusCode >= 400) {
            let parsed: { code?: string; retryable?: boolean } = {};
            try {
              parsed = JSON.parse(raw) as { code?: string; retryable?: boolean };
            } catch {
              // Preserve plain-text errors from older daemons.
            }
            reject(new DaemonHttpError(
              statusCode,
              parsed.code,
              parsed.retryable ?? (statusCode === undefined || statusCode >= 500),
              `HTTP ${statusCode}: ${this.errorMessage(statusCode, raw)}`,
            ));
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

  private errorMessage(statusCode: number | undefined, raw: string): string {
    return formatDaemonHttpError(statusCode, raw);
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

  // ── Shutdown ───────────────────────────────────────────────────

  shutdown(): Promise<{ success: boolean }> {
    return this.post<{ success: boolean }>('/shutdown');
  }

  // ── Project ───────────────────────────────────────────────────

  getProject(): Promise<ProjectState> {
    return this.get<ProjectState>('/project');
  }

  changeDir(dir: string): Promise<ChangeDirResponse> {
    return this.post<ChangeDirResponse>('/cd', { path: dir });
  }

  // ── Models ────────────────────────────────────────────────────

  listModels(): Promise<ModelInfo[]> {
    return this.get<ModelInfo[]>('/models');
  }

  setReasoningEffort(provider: string, effort: string | null): Promise<{ ok: boolean }> {
    return this.post<{ ok: boolean }>('/live/reasoning_effort', {
      provider,
      reasoning_effort: effort,
    });
  }

  getApprovalMode(): Promise<ApprovalModeResponse> {
    return this.get<ApprovalModeResponse>('/approval_mode');
  }

  setApprovalMode(mode: ApprovalMode): Promise<ApprovalModeResponse> {
    return this.post<ApprovalModeResponse>('/approval_mode', { mode });
  }

  sendPermissionDecision(
    sessionId: string,
    decision: PermissionDecision,
    toolName?: string,
  ): Promise<PermissionDecisionResponse> {
    return this.post<PermissionDecisionResponse>('/chat/permission', {
      session_id: sessionId,
      decision,
      ...(toolName ? { tool_name: toolName } : {}),
    });
  }

  // ── Skills ───────────────────────────────────────────────────

  listSkills(): Promise<SkillInfo[]> {
    return this.get<SkillInfo[]>('/skills');
  }

  // ── Config / Providers ───────────────────────────────────────

  getConfig(): Promise<ConfigResponse> {
    return this.get<ConfigResponse>('/config');
  }

  reloadConfig(): Promise<ConfigResponse> {
    return this.post<ConfigResponse>('/config/reload');
  }

  listProviders(): Promise<ProvidersResponse> {
    return this.get<ProvidersResponse>('/providers');
  }

  createProvider(req: CreateProviderRequest): Promise<ProviderInfo> {
    return this.post<ProviderInfo>('/providers', req);
  }

  patchProvider(name: string, req: PatchProviderRequest): Promise<ProviderInfo> {
    return this.patch<ProviderInfo>(`/providers/${encodeURIComponent(name)}`, req);
  }

  deleteProvider(name: string): Promise<ProvidersResponse> {
    return this.delete<ProvidersResponse>(`/providers/${encodeURIComponent(name)}`);
  }

  setDefaultProvider(name: string): Promise<ConfigResponse> {
    return this.post<ConfigResponse>(`/providers/${encodeURIComponent(name)}/default`);
  }

  patchThinking(name: string, req: PatchThinkingRequest): Promise<ProviderInfo> {
    return this.patch<ProviderInfo>(`/providers/${encodeURIComponent(name)}/thinking`, req);
  }

  // ── Auth / CodingPlan ────────────────────────────────────────

  authStatus(): Promise<AuthStatusResponse> {
    return this.get<AuthStatusResponse>('/auth/status');
  }

  startLogin(openBrowser = true): Promise<LoginStartResponse> {
    return this.post<LoginStartResponse>('/auth/login/start', {
      open_browser: openBrowser,
    });
  }

  pollLogin(loginId: string): Promise<LoginPollResponse> {
    return this.post<LoginPollResponse>(`/auth/login/${encodeURIComponent(loginId)}/poll`);
  }

  cancelLogin(loginId: string): Promise<{ success: boolean }> {
    return this.delete<{ success: boolean }>(`/auth/login/${encodeURIComponent(loginId)}`);
  }

  logout(): Promise<AuthStatusResponse> {
    return this.post<AuthStatusResponse>('/auth/logout');
  }

  setupCodingPlan(loginId?: string): Promise<CodingPlanSetupResponse> {
    return this.post<CodingPlanSetupResponse>('/codingplan/setup', { login_id: loginId });
  }

  // ── Sessions ──────────────────────────────────────────────────

  listSessions(): Promise<SessionMeta[]> {
    return this.get<SessionMeta[]>('/sessions');
  }

  listProjectSessions(projectHash: string): Promise<SessionMeta[]> {
    return this.get<SessionMeta[]>(`/projects/${encodeURIComponent(projectHash)}/sessions`);
  }

  listSessionsForWorkingDir(workingDir: string): Promise<SessionMeta[]> {
    return this.get<SessionMeta[]>(`/sessions/by-working-dir?working_dir=${encodeURIComponent(workingDir)}`);
  }

  resolveSession(id: string): Promise<SessionMeta> {
    return this.get<SessionMeta>(`/sessions/resolve/${encodeURIComponent(id)}`);
  }

  getSession(projectHash: string, id: string): Promise<SessionDetail> {
    return this.get<SessionDetail>(`/projects/${projectHash}/sessions/${id}`);
  }

  createSession(name?: string, workingDir?: string): Promise<CreateSessionResponse> {
    return this.post<CreateSessionResponse>('/sessions', {
      title: name,
      working_dir: workingDir,
    });
  }

  appendSessionMessages(
    sessionId: string,
    req: AppendSessionMessagesRequest,
  ): Promise<AppendSessionMessagesResponse> {
    return this.post<AppendSessionMessagesResponse>(
      `/sessions/${encodeURIComponent(sessionId)}/messages`,
      req,
    );
  }

  renameSession(projectHash: string, id: string, name: string): Promise<string> {
    return this.patch<string>(
      `/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(id)}/rename`,
      { name },
    );
  }

  deleteSession(projectHash: string, id: string): Promise<string> {
    return this.delete<string>(
      `/projects/${encodeURIComponent(projectHash)}/sessions/${encodeURIComponent(id)}`,
    );
  }

  searchSessions(query: string): Promise<SessionMeta[]> {
    return this.get<SessionMeta[]>(`/sessions/search?q=${encodeURIComponent(query)}`);
  }

  // ── Chat (SSE) ────────────────────────────────────────────────

  streamChat(req: ChatRequest, callbacks: ChatStreamCallbacks): AbortController {
    const controller = new AbortController();
    const payload = JSON.stringify(req);
    let terminalSeen = false;
    const guardedCallbacks: ChatStreamCallbacks = {
      ...callbacks,
      onDone: (...args) => {
        if (terminalSeen) return;
        terminalSeen = true;
        callbacks.onDone(...args);
      },
      onStopped: () => {
        if (terminalSeen) return;
        terminalSeen = true;
        callbacks.onStopped();
      },
      onError: (message) => {
        if (terminalSeen) return;
        terminalSeen = true;
        callbacks.onError(message);
      },
    };

    const token = readDaemonToken(this.port);
    const options: http.RequestOptions = {
      hostname: this.host,
      port: this.port,
      path: '/chat',
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Accept': 'text/event-stream',
        'X-AtomCode-Client': 'vscode',
        ...(token ? { Authorization: `Bearer ${token}` } : {}),
        'Content-Length': Buffer.byteLength(payload),
      },
    };

    const httpReq = http.request(options, (res) => {
      if (!res.statusCode || res.statusCode >= 400) {
        const chunks: Buffer[] = [];
        res.on('data', (chunk: Buffer) => chunks.push(chunk));
        res.on('end', () => {
          const raw = Buffer.concat(chunks).toString('utf-8');
          guardedCallbacks.onError(`HTTP ${res.statusCode}: ${this.errorMessage(res.statusCode, raw)}`);
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
            if (!terminalSeen) {
              this.handleSSEData(data, guardedCallbacks);
            }
          }
        }
      });

      res.on('end', () => {
        // Process any remaining data in the buffer
        if (!terminalSeen && buffer.trim().startsWith('data: ')) {
          const data = buffer.trim().slice(6);
          this.handleSSEData(data, guardedCallbacks);
        }
        if (!terminalSeen) {
          if (controller.signal.aborted) {
            guardedCallbacks.onStopped();
          } else {
            guardedCallbacks.onError('Stream ended before a terminal event');
          }
        }
      });

      res.on('error', (err) => {
        const classified = classifyDaemonStreamError(err.message, controller.signal.aborted);
        if (classified.type === 'stopped') {
          guardedCallbacks.onStopped();
        } else {
          guardedCallbacks.onError(classified.message);
        }
      });
    });

    httpReq.on('error', (err: NodeJS.ErrnoException) => {
      if (err.code === 'ECONNREFUSED') {
        // Try auto-reconnect for streaming too
        if (this.onConnectionLost) {
          this.onConnectionLost().then((restarted) => {
            if (restarted) {
              // Retry the stream by calling streamChat again
              const retryController = this.streamChat(req, callbacks);
              // Forward abort from original controller to retry.
              // Check if already aborted (event already fired) to avoid missing the signal.
              if (controller.signal.aborted) {
                retryController.abort();
              } else {
                controller.signal.addEventListener('abort', () => retryController.abort());
              }
            } else {
              guardedCallbacks.onError('Daemon not running');
            }
          }).catch(() => {
            guardedCallbacks.onError('Daemon not running');
          });
        } else {
          guardedCallbacks.onError('Daemon not running');
        }
      } else if (controller.signal.aborted) {
        // Intentional abort, don't report as error
        guardedCallbacks.onStopped();
      } else {
        guardedCallbacks.onError(`Connection error: ${err.message}`);
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
      case 'runtime_info':
        callbacks.onRuntimeInfo?.(event.provider, event.model);
        break;
      case 'mode':
        callbacks.onMode?.(event.mode);
        break;
      case 'text':
        callbacks.onText(event.content);
        break;
      case 'tool_batch':
        callbacks.onToolBatch(
          event.calls.map((c) => ({ id: c.id, name: c.name, args: c.arguments })),
        );
        break;
      case 'tool_start':
        callbacks.onToolStart(event.id, event.name, event.arguments);
        break;
      case 'tool_progress':
        callbacks.onToolProgress(event.id, event.progress);
        break;
      case 'tool_result':
        callbacks.onToolResult(event.id, event.name, event.output, event.success, event.duration_ms);
        break;
      case 'permission_request':
        callbacks.onPermissionRequest({
          sessionId: event.session_id,
          toolName: event.tool_name,
          reason: event.reason,
          callId: event.call_id,
          args: event.arguments,
        });
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
      case 'warning':
        callbacks.onWarning(event.message);
        break;
      case 'persistence_warning':
        callbacks.onPersistenceWarning?.(event.message);
        break;
      case 'rate_limited':
        callbacks.onRateLimited({
          message: event.message,
          retryAfterSeconds: event.retry_after_seconds,
          attempt: event.attempt,
          maxAttempts: event.max_attempts,
        });
        break;
      case 'done':
        callbacks.onDone(
          event.tokens,
          event.tool_calls,
          event.session_id,
          event.stop_reason,
          event.message,
        );
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

  stopGeneration(sessionId: string): Promise<{ success: boolean; message: string }> {
    return this.post<{ success: boolean; message: string }>('/chat/stop', { session_id: sessionId });
  }

  activeSessions(): Promise<string[]> {
    return this.get<string[]>('/chat/active');
  }
}
