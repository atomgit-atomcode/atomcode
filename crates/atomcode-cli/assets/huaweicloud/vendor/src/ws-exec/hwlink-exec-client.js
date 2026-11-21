/*
 * This file contains code derived from hwlink.
 *
 * Source:
 * https://gitcode.com/huawei-developers/hwlink
 *
 * Licensed under the ISC License.
 */

import {
  DEFAULT_TIMEOUT_MS,
  WebSocketExecError,
  buildMarkers,
  buildShellCommands,
  cleanCommandOutput,
} from './ws-exec-client.js';
import { HwlinkWebSocketMultiplexer } from './hwlink-multiplexer.js';
import { HwlinkTerminalChannel } from './hwlink-terminal-channel.js';

const decoder = new TextDecoder();
const encoder = new TextEncoder();

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

function normalizeSource(source) {
  const numericSource = Number(source);
  if (!Number.isInteger(numericSource) || numericSource < -0x80000000 || numericSource > 0xffffffff) {
    throw new WebSocketExecError('hwlink source must be an int32 or uint32 number', 2);
  }
  return numericSource;
}

function normalizeUrl(url) {
  if (!url || !String(url).trim()) {
    throw new WebSocketExecError('hwlink url is required', 2);
  }
  return String(url);
}

function createHwlinkTerminal(options = {}) {
  const {
    url,
    source,
    username = 'root',
    WebSocketImpl = globalThis.WebSocket,
    protocol = 'devenv',
    cols,
    rows,
    onFrame,
    onData,
    onError,
    onClose,
    trace = false,
  } = options;

  const normalizedUrl = normalizeUrl(url);
  const normalizedSource = normalizeSource(source);
  const mux = new HwlinkWebSocketMultiplexer(normalizedUrl, normalizedSource, {
    WebSocketImpl,
    protocol,
    onFrame,
    trace,
  });
  const term = new HwlinkTerminalChannel(username);
  let closed = false;
  let readySettled = false;
  let readyResolve;
  let readyReject;

  const ready = new Promise((resolve, reject) => {
    readyResolve = resolve;
    readyReject = reject;
  });

  function settleReady(error) {
    if (readySettled) return;
    readySettled = true;
    if (error) readyReject(error);
    else readyResolve(handle);
  }

  function handleError(error, prefix) {
    const wrapped = new WebSocketExecError(`${prefix}: ${error.message}`, 1, {
      cause: error,
      phase: readySettled ? 'open' : 'opening',
    });
    settleReady(wrapped);
    if (onError) onError(wrapped);
    close();
  }

  function handleClose() {
    settleReady(
      new WebSocketExecError('hwlink terminal closed before ready', 1, {
        phase: 'opening',
      }),
    );
    if (closed) return;
    closed = true;
    if (onClose) onClose();
  }

  term.onReady(() => {
    if (Number.isFinite(cols) && Number.isFinite(rows)) {
      term.resize(cols, rows);
    }
    settleReady();
  });
  term.onData((data) => {
    if (onData) onData(data);
  });
  term.onError((error) => handleError(error, 'hwlink terminal error'));
  term.onClose(handleClose);
  mux.onError = (error) => handleError(error, 'hwlink websocket error');
  mux.onClose = handleClose;

  function close() {
    if (closed) return;
    closed = true;
    term.close();
    mux.close();
  }

  const handle = {
    url: normalizedUrl,
    source: normalizedSource,
    username,
    mux,
    term,
    ready,
    close,
    resize: (nextCols, nextRows) => term.resize(nextCols, nextRows),
    sendInput: (data) => term.sendInput(data),
    sendText: (text) => term.sendText(text),
  };

  term.attach(mux);
  return handle;
}

class HwlinkTerminalExecSession {
  constructor(options = {}) {
    const {
      url,
      source,
      username = 'root',
      timeoutMs = DEFAULT_TIMEOUT_MS,
      WebSocketImpl = globalThis.WebSocket,
      protocol = 'devenv',
      cols,
      rows,
      onFrame,
      onData,
      trace = false,
    } = options;

    this.url = normalizeUrl(url);
    this.source = normalizeSource(source);
    this.username = username;
    this.timeoutMs = normalizeTimeout(timeoutMs);
    this.onData = onData;
    this.initialCols = cols;
    this.initialRows = rows;
    this.state = 'opening';
    this.readyBuffer = '';
    this.inputEchoed = false;
    this.pending = null;
    this.queue = Promise.resolve();
    this.readyMarkers = buildMarkers();
    const { readyCommand } = buildShellCommands(this.readyMarkers);
    this.readyCommand = readyCommand;

    this.readyPromise = new Promise((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });

    this.readyTimeout = setTimeout(() => {
      this.fail(
        new WebSocketExecError(`hwlink terminal ready timeout after ${this.timeoutMs}ms`, 124, {
          partialOutput: this.readyBuffer,
          phase: 'opening',
        }),
      );
    }, this.timeoutMs);

    try {
      this.mux = new HwlinkWebSocketMultiplexer(this.url, this.source, {
        WebSocketImpl,
        protocol,
        onFrame,
        trace,
      });
    } catch (error) {
      clearTimeout(this.readyTimeout);
      throw new WebSocketExecError(error.message, 2, { cause: error, phase: 'opening' });
    }

    this.term = new HwlinkTerminalChannel(username);
    this.term.onData((data) => this.handleTerminalData(data));
    this.term.onError((error) => {
      this.fail(
        new WebSocketExecError(`hwlink terminal error: ${error.message}`, 1, {
          cause: error,
          phase: this.state,
        }),
      );
    });
    this.term.onReady(() => {
      if (Number.isFinite(this.initialCols) && Number.isFinite(this.initialRows)) {
        this.term.resize(this.initialCols, this.initialRows);
      }
      this.sendLine(this.readyCommand);
    });
    this.term.onClose(() => {
      if (this.state !== 'closed') {
        this.fail(
          new WebSocketExecError('hwlink terminal closed before completion marker', 1, {
            phase: this.state,
          }),
        );
      }
    });
    this.mux.onClose = () => {
      if (this.state !== 'closed') {
        this.fail(
          new WebSocketExecError('hwlink websocket closed before completion marker', 1, {
            phase: this.state,
          }),
        );
      }
    };
    this.mux.onError = (error) => {
      if (this.state !== 'closed') {
        this.fail(
          new WebSocketExecError(`hwlink websocket error: ${error.message}`, 1, {
            cause: error,
            phase: this.state,
          }),
        );
      }
    };

    this.term.attach(this.mux);
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

  resize(cols, rows) {
    this.term.resize(cols, rows);
  }

  close() {
    if (this.state === 'closed') return;
    const wasOpening = this.state === 'opening';
    const pending = this.pending;
    this.state = 'closed';
    clearTimeout(this.readyTimeout);
    this.pending = null;

    if (wasOpening) {
      this.rejectReady(
        new WebSocketExecError('hwlink terminal session closed before ready', 1, {
          phase: 'opening',
        }),
      );
    }

    if (pending) {
      clearTimeout(pending.timeout);
      pending.reject(
        new WebSocketExecError('hwlink terminal session closed before completion marker', 1, {
          phase: 'running',
        }),
      );
    }

    this.term.close();
    this.mux.close();
  }

  fail(error) {
    if (this.state === 'closed') return;

    const wasOpening = this.state === 'opening';
    const pending = this.pending;
    this.state = 'closed';
    this.pending = null;
    clearTimeout(this.readyTimeout);

    if (wasOpening) {
      this.rejectReady(error);
    }

    if (pending) {
      clearTimeout(pending.timeout);
      pending.reject(error);
    }

    this.term.close();
    this.mux.close();
  }

  handleTerminalData(data) {
    if (this.state === 'closed') return;

    const chunk = decoder.decode(data, { stream: true });
    if (this.onData) this.onData(data, chunk);

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
      return Promise.reject(
        new WebSocketExecError('hwlink terminal exec session is not ready', 1, {
          phase: this.state,
        }),
      );
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
            new WebSocketExecError(`hwlink terminal exec timeout after ${timeoutMs}ms`, 124, {
              partialOutput: this.pending ? this.pending.buffer : '',
              phase: 'running',
            }),
          );
        }, timeoutMs),
      };

      this.sendLine(shellCommand);
      this.sendLine(doneCommand);
    });
  }

  tryCompletePending() {
    const pending = this.pending;
    if (!pending) return;

    const doneMarker = pending.markers.doneMarker;
    const markerIndex = pending.buffer.lastIndexOf(doneMarker);
    if (markerIndex === -1) return;

    const afterMarker = pending.buffer.slice(markerIndex + doneMarker.length);
    const exitMatch = afterMarker.match(/^(\d+)/);
    if (!exitMatch) return;

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
      source: this.source,
      username: this.username,
      command: pending.command,
    });
  }

  sendLine(line) {
    this.term.sendInput(encoder.encode(`${line}\n`));
  }
}

async function connectHwlinkTerminalSession(options = {}) {
  const session = new HwlinkTerminalExecSession(options);
  await session.ready();
  return session;
}

async function connectHwlinkInteractiveTerminal(options = {}) {
  const terminal = createHwlinkTerminal(options);
  await terminal.ready;
  return terminal;
}

async function executeHwlinkCommand(options = {}) {
  const { command, timeoutMs = DEFAULT_TIMEOUT_MS, ...sessionOptions } = options;
  const shellCommand = normalizeCommand(command);
  const normalizedTimeoutMs = normalizeTimeout(timeoutMs);
  const session = await connectHwlinkTerminalSession({
    ...sessionOptions,
    timeoutMs: normalizedTimeoutMs,
  });
  try {
    return await session.exec(shellCommand, { timeoutMs: normalizedTimeoutMs });
  } finally {
    session.close();
  }
}

export {
  HwlinkTerminalExecSession,
  connectHwlinkInteractiveTerminal,
  connectHwlinkTerminalSession,
  createHwlinkTerminal,
  executeHwlinkCommand,
  normalizeSource,
};
