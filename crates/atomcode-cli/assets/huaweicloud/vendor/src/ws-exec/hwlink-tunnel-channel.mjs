/*
 * This file contains code derived from hwlink.
 *
 * Source:
 * https://gitcode.com/huawei-developers/hwlink
 *
 * Licensed under the ISC License.
 */

import { createServer } from 'node:net';

import {
  OpCode,
  ReserveSource,
  FIXED_HEADER_LEN,
  MAX_SEND_CHUNK_SIZE,
  createPacket,
  isOpTunnelSuccess,
  isOpTcpTunnelData,
  isOpFailed,
  isSubStreamPing,
  nextIdentifier,
} from './hwlink-packet.js';

const MAX_TUNNEL_PAYLOAD_SIZE = MAX_SEND_CHUNK_SIZE - FIXED_HEADER_LEN;

class HwlinkTunnelChannel {
  constructor({ localPort = 0, remotePort, onReady, onClose, onError }) {
    this.localPort = localPort;
    this.remotePort = remotePort;
    this.identifier = nextIdentifier();
    this.mux = null;
    this.closed = false;
    this.opened = false;

    this.subConnections = new Map();
    this.localServer = null;
    this.nextSubId = 1;

    this.onReadyCb = onReady || null;
    this.onCloseCb = onClose || null;
    this.onErrorCb = onError || null;

    this._readyResolve = null;
    this._readyReject = null;
    this.ready = new Promise((resolve, reject) => {
      this._readyResolve = resolve;
      this._readyReject = reject;
    });
  }

  attach(mux) {
    this.mux = mux;
    mux.register(this);
  }

  onopen() {
    if (this.closed || this.opened) return;
    this.opened = true;
    this.startLocalServer();
  }

  onmessage(packet) {
    if (this.closed) return;

    if (isOpFailed(packet.operation)) {
      const subConn = this.subConnections.get(packet.identifier);
      if (subConn && subConn.socket && !subConn.socket.destroyed) {
        try {
          subConn.socket.end();
        } catch {}
      }
      this.subConnections.delete(packet.identifier);
      this._unregisterSubId(packet.identifier);
      return;
    }

    if (isOpTunnelSuccess(packet.operation)) {
      const subConn = this.subConnections.get(packet.identifier);
      if (subConn && !subConn.ready) {
        subConn.ready = true;
        if (subConn.readyResolve) subConn.readyResolve();
      }
      if (packet.identifier === this.identifier && this._readyResolve) {
        this._readyResolve();
        this._readyResolve = null;
        this._readyReject = null;
      }
      return;
    }

    if (isOpTcpTunnelData(packet.operation)) {
      const subConn = this.subConnections.get(packet.identifier);
      if (subConn && subConn.socket && !subConn.socket.destroyed) {
        subConn.socket.write(packet.data);
      }
      return;
    }

    if (isSubStreamPing(packet.operation)) {
      this.sendRaw(
        createPacket({
          operation: OpCode.OpTcpTunnelData | OpCode.OpSubStreamPong,
          reserve: ReserveSource.Web,
          identifier: packet.identifier,
          source: this.mux ? this.mux.source : 0,
          payload: new Uint8Array(0),
        }),
      );
      return;
    }

    if (packet.operation === OpCode.OpDisconnect) {
      const subConn = this.subConnections.get(packet.identifier);
      if (subConn && subConn.socket && !subConn.socket.destroyed) {
        subConn.socket.end();
      }
      this.subConnections.delete(packet.identifier);
      this._unregisterSubId(packet.identifier);
      return;
    }
  }

  onerror(error) {
    if (this.onErrorCb) this.onErrorCb(error);
    if (this._readyReject) {
      this._readyReject(error);
      this._readyResolve = null;
      this._readyReject = null;
    }
  }

  onclose() {
    this.close();
  }

  startLocalServer() {
    this.localServer = createServer((socket) => {
      this.onIncomingConnection(socket);
    });

    this.localServer.listen(this.localPort, '127.0.0.1', () => {
      this.localPort = this.localServer.address().port;
      if (this.onReadyCb) this.onReadyCb();
      if (this._readyResolve) {
        this._readyResolve();
        this._readyResolve = null;
        this._readyReject = null;
      }
    });

    this.localServer.on('error', (err) => {
      if (this.onErrorCb) this.onErrorCb(err);
      if (this._readyReject) {
        this._readyReject(err);
        this._readyResolve = null;
        this._readyReject = null;
      }
    });
  }

  _registerSubId(subId) {
    if (this.mux && !this.mux.channels.has(subId)) {
      this.mux.channels.set(subId, this);
      this.mux.queue.register({ identifier: subId });
    }
  }

  _unregisterSubId(subId) {
    if (this.mux) {
      this.mux.channels.delete(subId);
      this.mux.queue.unregister({ identifier: subId });
    }
  }

  onIncomingConnection(socket) {
    const subId = this.nextSubId;
    this.nextSubId = ((this.nextSubId + 1) & 0xffffffff) >>> 0;

    let readyResolve;
    const readyPromise = new Promise((resolve) => {
      readyResolve = resolve;
    });
    const subConn = {
      identifier: subId,
      socket,
      ready: false,
      readyResolve,
      readyPromise,
    };

    this.subConnections.set(subId, subConn);
    this._registerSubId(subId);

    this.sendRaw(
      createPacket({
        operation: OpCode.OpCreateTcpTunnel,
        reserve: ReserveSource.Web,
        srcPort: this.localPort,
        dstPort: this.remotePort,
        identifier: subId,
        source: this.mux ? this.mux.source : 0,
        payload: new Uint8Array(0),
      }),
    );

    socket.on('data', (data) => {
      if (this.closed || socket.destroyed) return;
      for (let offset = 0; offset < data.length; offset += MAX_TUNNEL_PAYLOAD_SIZE) {
        const payload = data.subarray(offset, offset + MAX_TUNNEL_PAYLOAD_SIZE);
        this.sendRaw(
          createPacket({
            operation: OpCode.OpTcpTunnelData,
            reserve: ReserveSource.Web,
            srcPort: this.localPort,
            dstPort: this.remotePort,
            identifier: subId,
            source: this.mux ? this.mux.source : 0,
            payload,
          }),
        );
      }
    });

    socket.on('close', () => {
      this._unregisterSubId(subId);
      this.subConnections.delete(subId);
    });

    socket.on('error', () => {
      this._unregisterSubId(subId);
      this.subConnections.delete(subId);
    });
  }

  destroySubConnection(subConn, error) {
    if (subConn.socket && !subConn.socket.destroyed) {
      if (error) subConn.socket.destroy(error);
      else subConn.socket.destroy();
    }
    this._unregisterSubId(subConn.identifier);
    this.subConnections.delete(subConn.identifier);
  }

  sendRaw(data) {
    if (this.mux) this.mux.sendFairly(this, data);
  }

  close() {
    if (this.closed) return;
    this.closed = true;

    for (const subConn of this.subConnections.values()) {
      this.destroySubConnection(subConn);
    }
    this.subConnections.clear();

    if (this.localServer) {
      this.localServer.close();
      this.localServer = null;
    }

    if (this.mux) this.mux.unregister(this);
    if (this.onCloseCb) this.onCloseCb();
  }
}

export { HwlinkTunnelChannel };
