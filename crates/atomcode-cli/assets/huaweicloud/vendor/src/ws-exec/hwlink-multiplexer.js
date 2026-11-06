/*
 * This file contains code derived from hwlink.
 *
 * Source:
 * https://gitcode.com/huawei-developers/hwlink
 *
 * Licensed under the ISC License.
 */

import { Buffer } from 'node:buffer';

import { FairQueue } from './hwlink-fair-queue.js';
import { formatPacketOneLine, parsePacket } from './hwlink-packet.js';

const WS_OPEN = 1;

function addWsListener(ws, event, handler) {
  if (typeof ws.on === 'function') {
    ws.on(event, handler);
    return;
  }
  if (typeof ws.addEventListener === 'function') {
    ws.addEventListener(event, handler);
    return;
  }
  throw new Error('WebSocket implementation does not support event listeners');
}

function closeQuietly(ws) {
  try {
    ws.close();
  } catch {
    // Close is best-effort during error settlement.
  }
}

function extractMessageData(eventOrData) {
  if (
    eventOrData &&
    typeof eventOrData === 'object' &&
    !Buffer.isBuffer(eventOrData) &&
    !(eventOrData instanceof ArrayBuffer) &&
    !ArrayBuffer.isView(eventOrData) &&
    'data' in eventOrData
  ) {
    return eventOrData.data;
  }
  return eventOrData;
}

function eventDataToUint8Array(data) {
  if (data instanceof Uint8Array) return Promise.resolve(data);
  if (Buffer.isBuffer(data)) {
    return Promise.resolve(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  }
  if (data instanceof ArrayBuffer) return Promise.resolve(new Uint8Array(data));
  if (ArrayBuffer.isView(data)) {
    return Promise.resolve(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  }
  if (data && typeof data.arrayBuffer === 'function') {
    return data.arrayBuffer().then((buffer) => new Uint8Array(buffer));
  }
  return Promise.reject(new Error(`unsupported websocket message data: ${typeof data}`));
}

function sendBinary(ws, data) {
  if (ws.readyState !== WS_OPEN) return Promise.resolve();

  return new Promise((resolve, reject) => {
    let settled = false;
    const done = (err) => {
      if (settled) return;
      settled = true;
      if (err) reject(err);
      else resolve();
    };

    try {
      const maybePromise = ws.send(data, done);
      if (maybePromise && typeof maybePromise.then === 'function') {
        maybePromise.then(() => done(), done);
        return;
      }
      if (ws.send.length < 2) done();
    } catch (error) {
      done(error);
    }
  });
}

class HwlinkWebSocketMultiplexer {
  constructor(url, source, options = {}) {
    const { WebSocketImpl = globalThis.WebSocket, protocol = 'devenv', onFrame, trace = false } = options;

    if (typeof WebSocketImpl !== 'function') {
      throw new TypeError('global WebSocket is unavailable; use Node.js 22+ or pass WebSocketImpl');
    }
    if (!Number.isInteger(source) || source < -0x80000000 || source > 0xffffffff) {
      throw new Error('hwlink source must be an int32 or uint32 number');
    }

    this.url = url;
    this.source = source;
    this.onFrame = onFrame;
    this.trace = trace;
    this.channels = new Map();
    this.unknownIdentifierHandler = null;
    this.onClose = null;
    this.onError = null;
    this.closed = false;
    this.queue = new FairQueue((data) => this.wsSend(data));
    this.ws = new WebSocketImpl(url, protocol);

    if ('binaryType' in this.ws) {
      this.ws.binaryType = 'arraybuffer';
    }

    addWsListener(this.ws, 'open', () => {
      for (const ch of this.channels.values()) ch.onopen();
    });

    addWsListener(this.ws, 'message', (eventOrData) => {
      this.handleMessage(extractMessageData(eventOrData)).catch((error) => {
        this.handleError(error);
      });
    });

    addWsListener(this.ws, 'error', (eventOrError) => {
      const error = eventOrError instanceof Error ? eventOrError : new Error('hwlink websocket error');
      this.handleError(error);
    });

    addWsListener(this.ws, 'close', (eventOrCode, maybeReason) => {
      this.closed = true;
      const closeEvent = this.normalizeCloseEvent(eventOrCode, maybeReason);
      for (const ch of this.channels.values()) ch.onclose(closeEvent);
      if (this.onClose) this.onClose(closeEvent);
    });
  }

  normalizeCloseEvent(eventOrCode, maybeReason) {
    if (typeof eventOrCode === 'number') {
      return {
        code: eventOrCode,
        reason: Buffer.isBuffer(maybeReason) ? maybeReason.toString() : String(maybeReason || ''),
      };
    }

    return {
      code: eventOrCode && typeof eventOrCode.code === 'number' ? eventOrCode.code : 0,
      reason: eventOrCode && eventOrCode.reason ? String(eventOrCode.reason) : '',
    };
  }

  register(ch) {
    this.channels.set(ch.identifier, ch);
    this.queue.register(ch);
    if (this.ws.readyState === WS_OPEN) {
      ch.onopen();
    }
  }

  unregister(ch) {
    this.channels.delete(ch.identifier);
    this.queue.unregister(ch);
  }

  sendFairly(ch, data) {
    this.queue.sendFairly(ch, data);
  }

  sendImmediately(data) {
    this.queue.sendImmediately(data);
  }

  get readyState() {
    return this.ws.readyState;
  }

  async handleMessage(data) {
    const bytes = await eventDataToUint8Array(data);
    const packet = parsePacket(bytes);
    this.reportFrame('in', packet);

    const ch = this.channels.get(packet.identifier);
    if (ch) {
      ch.onmessage(packet);
      return;
    }

    if (this.unknownIdentifierHandler) {
      this.unknownIdentifierHandler(packet, this);
    }
  }

  wsSend(data) {
    const packet = parsePacket(data);
    this.reportFrame('out', packet);
    return sendBinary(this.ws, data);
  }

  reportFrame(direction, packet) {
    if (this.onFrame) {
      this.onFrame({ direction, packet, line: formatPacketOneLine(packet) });
    }
    if (this.trace) {
      const arrow = direction === 'in' ? '<-' : '->';
      console.error(`${arrow} ${formatPacketOneLine(packet)}`);
    }
  }

  handleError(error) {
    for (const ch of this.channels.values()) ch.onerror(error);
    if (this.onError) this.onError(error);
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    closeQuietly(this.ws);
  }
}

export { HwlinkWebSocketMultiplexer, addWsListener, closeQuietly, eventDataToUint8Array };
