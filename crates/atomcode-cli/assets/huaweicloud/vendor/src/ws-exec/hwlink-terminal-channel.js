/*
 * This file contains code derived from hwlink.
 *
 * Source:
 * https://gitcode.com/huawei-developers/hwlink
 *
 * Licensed under the ISC License.
 */

import {
  OpCode,
  ReserveSource,
  FIXED_HEADER_LEN,
  MAX_SEND_CHUNK_SIZE,
  createPacket,
  isOpCmdTerminalData,
  isOpFailed,
  isSubStreamPing,
  nextIdentifier,
} from './hwlink-packet.js';

const encoder = new TextEncoder();
const MAX_TERMINAL_PAYLOAD_SIZE = MAX_SEND_CHUNK_SIZE - FIXED_HEADER_LEN;

class HwlinkTerminalChannel {
  constructor(username = 'root') {
    this.username = username;
    this.identifier = nextIdentifier();
    this.mux = null;
    this.closed = false;
    this.opened = false;
    this.onDataCb = null;
    this.onCloseCb = null;
    this.onErrorCb = null;
    this.onReadyCb = null;
  }

  onData(cb) {
    this.onDataCb = cb;
  }

  onClose(cb) {
    this.onCloseCb = cb;
  }

  onError(cb) {
    this.onErrorCb = cb;
  }

  onReady(cb) {
    this.onReadyCb = cb;
  }

  get isClosed() {
    return this.closed;
  }

  attach(mux) {
    this.mux = mux;
    mux.register(this);
  }

  onopen() {
    if (this.closed || this.opened) return;
    this.opened = true;
    this.sendRaw(
      createPacket({
        operation: OpCode.OpNewCmdTerminal,
        reserve: ReserveSource.Web,
        identifier: this.identifier,
        source: this.mux ? this.mux.source : 0,
        payload: encoder.encode(this.username),
      }),
    );
    if (this.onReadyCb) this.onReadyCb();
  }

  onmessage(packet) {
    if (this.closed) return;

    if (isOpFailed(packet.operation)) {
      this.handleError(new Error(`hwlink terminal failed: 0x${packet.operation.toString(16)}`));
      return;
    }

    if (isSubStreamPing(packet.operation)) {
      this.sendRaw(
        createPacket({
          operation: OpCode.OpCmdTerminalData | OpCode.OpSubStreamPong,
          reserve: ReserveSource.Web,
          identifier: this.identifier,
          source: this.mux ? this.mux.source : 0,
          payload: new Uint8Array(0),
        }),
      );
      return;
    }

    if (isOpCmdTerminalData(packet.operation) && packet.data) {
      if (this.onDataCb) this.onDataCb(packet.data);
    }
  }

  onerror(error) {
    if (this.onErrorCb) this.onErrorCb(error);
  }

  onclose() {
    this.close();
  }

  sendInput(data) {
    if (this.closed) return;
    for (let offset = 0; offset < data.byteLength; offset += MAX_TERMINAL_PAYLOAD_SIZE) {
      const payload = data.subarray(offset, offset + MAX_TERMINAL_PAYLOAD_SIZE);
      this.sendTerminalData(payload);
    }
  }

  sendTerminalData(payload) {
    this.sendRaw(
      createPacket({
        operation: OpCode.OpCmdTerminalData | OpCode.OpSubStreamPong,
        reserve: ReserveSource.Web,
        identifier: this.identifier,
        source: this.mux ? this.mux.source : 0,
        payload,
      }),
    );
  }

  sendText(text) {
    this.sendInput(encoder.encode(text));
  }

  resize(cols, rows) {
    if (this.closed) return;
    const payload = new Uint8Array(4);
    const view = new DataView(payload.buffer);
    view.setUint16(0, rows, false);
    view.setUint16(2, cols, false);
    this.sendRaw(
      createPacket({
        operation: OpCode.OpCmdTerminalResize,
        reserve: ReserveSource.Web,
        identifier: this.identifier,
        source: this.mux ? this.mux.source : 0,
        payload,
      }),
    );
  }

  sendRaw(data) {
    if (this.mux) this.mux.sendFairly(this, data);
  }

  handleError(error) {
    if (this.closed) return;
    this.closed = true;
    if (this.mux) this.mux.unregister(this);
    if (this.onErrorCb) this.onErrorCb(error);
    if (this.onCloseCb) this.onCloseCb();
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    if (this.mux) this.mux.unregister(this);
    if (this.onCloseCb) this.onCloseCb();
  }
}

export { HwlinkTerminalChannel };
