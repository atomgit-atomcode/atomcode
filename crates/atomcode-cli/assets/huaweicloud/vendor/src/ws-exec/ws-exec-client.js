import { Buffer } from 'node:buffer';
import { randomBytes } from 'node:crypto';

const DEFAULT_URL = 'ws://127.0.0.1:8080';
const DEFAULT_TIMEOUT_MS = 30000;

class WebSocketExecError extends Error {
  constructor(message, exitCode, details = {}) {
    super(message);
    this.name = 'WebSocketExecError';
    this.exitCode = exitCode;
    Object.assign(this, details);
  }
}

async function eventDataToString(data) {
  if (typeof data === 'string') return data;
  if (Buffer.isBuffer(data)) return data.toString('utf8');
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('utf8');
  if (ArrayBuffer.isView(data)) {
    return Buffer.from(data.buffer, data.byteOffset, data.byteLength).toString('utf8');
  }
  if (data && typeof data.arrayBuffer === 'function') {
    return Buffer.from(await data.arrayBuffer()).toString('utf8');
  }
  return String(data);
}

function sendLine(ws, line) {
  ws.send(`${line}\n`);
}

function closeQuietly(ws) {
  try {
    ws.close();
  } catch {
    // The caller is already settling the operation; close errors are not useful.
  }
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function stripEchoedLine(text, line, options = {}) {
  const escaped = escapeRegExp(line);
  const stripped = text.replace(new RegExp(`(^|\\r?\\n)${escaped}\\r?\\n?`), '$1');
  if (!options.allowAttached || stripped !== text) return stripped;
  return text.replace(new RegExp(`${escaped}(?:\\r?\\n){0,2}$`), '');
}

function buildMarkers(nonce = randomBytes(8).toString('hex')) {
  const readyPrefix = '__WS_EXEC_READY_';
  const donePrefix = '__WS_EXEC_DONE_';
  const readySuffix = `${nonce}__`;
  const doneSuffix = `${nonce}__:`;
  const readyMarker = `${readyPrefix}${readySuffix}`;
  const doneMarker = `${donePrefix}${doneSuffix}`;

  return {
    nonce,
    readyPrefix,
    donePrefix,
    readySuffix,
    doneSuffix,
    readyMarker,
    doneMarker,
  };
}

function buildShellCommands(markers) {
  return {
    readyCommand: `stty -echo 2>/dev/null; export PS1= PS2= PROMPT_COMMAND=; printf '\\n%s%s\\n' '${markers.readyPrefix}' '${markers.readySuffix}'`,
    doneCommand: `__ws_exec_rc=$?; printf '\\n%s%s%d\\n' '${markers.donePrefix}' '${markers.doneSuffix}' "$__ws_exec_rc"`,
  };
}

function cleanCommandOutput(output, { inputEchoed, command, doneCommand }) {
  let cleaned = output;
  if (inputEchoed) cleaned = stripEchoedLine(cleaned, command);
  cleaned = stripEchoedLine(cleaned, doneCommand, { allowAttached: true });
  return cleaned.replace(/^\r?\n/, '').replace(/\r?\n\r?\n$/, '\n');
}

function normalizeCommand(command) {
  if (!command || !String(command).trim()) {
    throw new WebSocketExecError('missing command', 2);
  }
  return String(command).trim();
}

function normalizeTimeout(timeoutMs) {
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
    throw new WebSocketExecError('timeoutMs must be a positive number of milliseconds', 2);
  }
  return timeoutMs;
}

class WebSocketShellSession {
  constructor(options = {}) {
    const {
      url = DEFAULT_URL,
      timeoutMs = DEFAULT_TIMEOUT_MS,
      WebSocketImpl = globalThis.WebSocket,
      onFrame,
    } = options;

    if (typeof WebSocketImpl !== 'function') {
      throw new WebSocketExecError('global WebSocket is unavailable; use Node.js 22+ or pass WebSocketImpl', 2);
    }

    this.url = url;
    this.timeoutMs = normalizeTimeout(timeoutMs);
    this.WebSocketImpl = WebSocketImpl;
    this.onFrame = onFrame;
    this.state = 'opening';
    this.readyBuffer = '';
    this.inputEchoed = false;
    this.pending = null;
    this.queue = Promise.resolve();
    this.ws = new WebSocketImpl(url);

    this.readyMarkers = buildMarkers();
    const { readyCommand } = buildShellCommands(this.readyMarkers);
    this.readyCommand = readyCommand;

    this.readyPromise = new Promise((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });

    this.readyTimeout = setTimeout(() => {
      this.fail(
        new WebSocketExecError(`exec ready timeout after ${this.timeoutMs}ms`, 124, {
          partialOutput: this.readyBuffer,
          phase: 'opening',
        }),
      );
    }, this.timeoutMs);

    this.ws.addEventListener('open', () => {
      sendLine(this.ws, this.readyCommand);
    });

    this.ws.addEventListener('message', (event) => {
      this.handleMessage(event).catch((error) => {
        this.fail(
          new WebSocketExecError(`exec message handling error: ${error.message}`, 1, {
            cause: error,
            phase: this.state,
          }),
        );
      });
    });

    this.ws.addEventListener('close', () => {
      if (this.state !== 'closed') {
        this.fail(new WebSocketExecError('exec websocket closed before completion marker', 1, { phase: this.state }));
      }
    });

    this.ws.addEventListener('error', () => {
      if (this.state !== 'closed') {
        this.fail(new WebSocketExecError('exec websocket error', 1, { phase: this.state }));
      }
    });
  }

  ready() {
    return this.readyPromise;
  }

  exec(command, options = {}) {
    const run = () => this.runExec(command, options);
    const result = this.queue.then(run, run);
    this.queue = result.catch(() => {});
    return result;
  }

  close() {
    if (this.state === 'closed') return;
    const wasOpening = this.state === 'opening';
    const pending = this.pending;
    this.state = 'closed';
    clearTimeout(this.readyTimeout);
    this.pending = null;

    if (wasOpening) {
      this.rejectReady(new WebSocketExecError('exec session closed before ready', 1, { phase: 'opening' }));
    }

    if (pending) {
      clearTimeout(pending.timeout);
      pending.reject(new WebSocketExecError('exec session closed before completion marker', 1, { phase: 'running' }));
    }

    closeQuietly(this.ws);
  }

  fail(error) {
    if (this.state === 'closed') return;

    const wasOpening = this.state === 'opening';
    const pending = this.pending;
    this.state = 'closed';
    this.pending = null;
    clearTimeout(this.readyTimeout);
    closeQuietly(this.ws);

    if (wasOpening) {
      this.rejectReady(error);
    }

    if (pending) {
      clearTimeout(pending.timeout);
      pending.reject(error);
    }
  }

  async handleMessage(event) {
    if (this.state === 'closed') return;

    const chunk = await eventDataToString(event.data);
    if (this.onFrame) this.onFrame(chunk);

    if (this.state === 'opening') {
      this.readyBuffer += chunk;
      this.tryCompleteReady();
      return;
    }

    if (this.pending) {
      this.pending.buffer += chunk;
      this.tryCompletePending();
    }
  }

  tryCompleteReady() {
    const readyIndex = this.readyBuffer.indexOf(this.readyMarkers.readyMarker);
    if (readyIndex === -1) return;

    this.inputEchoed = this.readyBuffer.slice(0, readyIndex).includes(this.readyCommand);
    this.readyBuffer = '';
    this.state = 'ready';
    clearTimeout(this.readyTimeout);
    this.resolveReady(this);
  }

  runExec(command, options = {}) {
    if (this.state !== 'ready') {
      return Promise.reject(new WebSocketExecError('exec session is not ready', 1, { phase: this.state }));
    }

    const shellCommand = normalizeCommand(command);
    const timeoutMs = normalizeTimeout(options.timeoutMs === undefined ? this.timeoutMs : options.timeoutMs);
    const markers = buildMarkers();
    const { doneCommand } = buildShellCommands(markers);

    return new Promise((resolve, reject) => {
      this.pending = {
        buffer: '',
        command: shellCommand,
        doneCommand,
        markers,
        reject,
        resolve,
        timeout: setTimeout(() => {
          this.fail(
            new WebSocketExecError(`exec timeout after ${timeoutMs}ms`, 124, {
              partialOutput: this.pending ? this.pending.buffer : '',
              phase: 'running',
            }),
          );
        }, timeoutMs),
      };

      sendLine(this.ws, shellCommand);
      sendLine(this.ws, doneCommand);
    });
  }

  tryCompletePending() {
    const pending = this.pending;
    if (!pending) return;

    const doneMarker = pending.markers.doneMarker;
    const markerIndex = pending.buffer.lastIndexOf(doneMarker);
    if (markerIndex === -1) return;

    // eslint-disable-next-line unicorn/no-unsafe-buffer-conversion -- pending.buffer is a String, not a Buffer
    const afterMarker = pending.buffer.slice(markerIndex + doneMarker.length);
    const exitMatch = afterMarker.match(/^(\d+)/);
    if (!exitMatch) return;

    // eslint-disable-next-line unicorn/no-unsafe-buffer-conversion -- pending.buffer is a String, not a Buffer
    const rawOutput = pending.buffer.slice(0, markerIndex);
    const stdout = cleanCommandOutput(rawOutput, {
      inputEchoed: this.inputEchoed,
      command: pending.command,
      doneCommand: pending.doneCommand,
    });

    clearTimeout(pending.timeout);
    this.pending = null;
    pending.resolve({
      stdout,
      exitCode: Number(exitMatch[1]),
      url: this.url,
      command: pending.command,
    });
  }
}

async function connectShellSession(options = {}) {
  const session = new WebSocketShellSession(options);
  await session.ready();
  return session;
}

async function executeCommand(options = {}) {
  const { command, timeoutMs = DEFAULT_TIMEOUT_MS, ...sessionOptions } = options;
  const shellCommand = normalizeCommand(command);
  const normalizedTimeoutMs = normalizeTimeout(timeoutMs);
  const session = await connectShellSession({ ...sessionOptions, timeoutMs: normalizedTimeoutMs });
  try {
    return await session.exec(shellCommand, { timeoutMs: normalizedTimeoutMs });
  } finally {
    session.close();
  }
}

export {
  DEFAULT_TIMEOUT_MS,
  DEFAULT_URL,
  WebSocketExecError,
  WebSocketShellSession,
  buildMarkers,
  buildShellCommands,
  cleanCommandOutput,
  connectShellSession,
  eventDataToString,
  executeCommand,
  stripEchoedLine,
};
