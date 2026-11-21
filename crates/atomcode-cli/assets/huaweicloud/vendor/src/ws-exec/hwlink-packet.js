/*
 * This file contains code derived from hwlink.
 *
 * Source:
 * https://gitcode.com/huawei-developers/hwlink
 *
 * Licensed under the ISC License.
 */

const FIXED_HEADER_LEN = 20;
const MAX_SEND_CHUNK_SIZE = 8 * 1024;

const OpCode = Object.freeze({
  OpCreateTcpTunnel: 0x03,
  OpTcpTunnelData: 0x04,
  OpListenTcpTunnel: 0x05,
  OpReverseCreateTcpTunnel: 0x43,
  OpCmdTerminalResize: 0xfa,
  OpNewCmdTerminal: 0xfb,
  OpCmdTerminalInit: 0xfc,
  OpCmdTerminalData: 0xfd,

  OpFileSuccess: 0x03 << 8,
  OpSubStreamPing: 0x06 << 8,
  OpSubStreamPong: 0x07 << 8,
  OpTunnelSuccess: 0x08 << 8,

  OpDisconnect: 0x03 << 16,

  ErrCmdTerminal: 0x0d << 24,
  ErrTcpTunnel: 0x0e << 24,
});

const ReserveSource = Object.freeze({
  Web: 0x00,
});

let nextId = 1;

function nextIdentifier() {
  nextId = ((nextId + 1) & 0xffffffff) >>> 0;
  return nextId;
}

function isOpTunnelSuccess(op) {
  return (op & 0xff00) === OpCode.OpTunnelSuccess;
}

function isOpTcpTunnelData(op) {
  return (op & 0xff) === OpCode.OpTcpTunnelData;
}

function isOpFailed(op) {
  return op >> 24 !== 0;
}

function isOpCreateTcpTunnel(op) {
  return (op & 0xff) === OpCode.OpCreateTcpTunnel;
}

function isOpReverseCreateTcpTunnel(op) {
  return (op & 0xff) === OpCode.OpReverseCreateTcpTunnel;
}

function isOpCmdTerminalData(op) {
  return (op & 0xff) === OpCode.OpCmdTerminalData;
}

function isOpListenTcpTunnel(op) {
  return (op & 0xff) === OpCode.OpListenTcpTunnel;
}

function isSubStreamPing(op) {
  return (op & 0xff00) === OpCode.OpSubStreamPing;
}

function hex2(n) {
  return `0x${n.toString(16).padStart(2, '0').toUpperCase()}`;
}

function operationToString(op) {
  return [hex2((op >> 24) & 0xff), hex2((op >> 16) & 0xff), hex2((op >> 8) & 0xff), hex2(op & 0xff)].join(' ');
}

function formatPacketOneLine(packet) {
  const err = (packet.operation >> 24) & 0xff;
  const ctl = (packet.operation >> 16) & 0xff;
  const sync = (packet.operation >> 8) & 0xff;
  const data = packet.operation & 0xff;
  const dataLen = packet.data ? packet.data.length : 0;
  return (
    `[${String(packet.headerLength).padStart(2)}]` +
    `[${hex2(packet.reserve)}]` +
    `[${String(packet.packetLength).padStart(5)}]` +
    `[${hex2(err)} ${hex2(ctl)} ${hex2(sync)} ${hex2(data)}]` +
    `[${String(packet.srcPort).padStart(5)}]` +
    `[${String(packet.dstPort).padStart(5)}]` +
    `[${String(packet.identifier).padStart(10)}]` +
    `[${String(packet.source).padStart(10)}]` +
    `[${String(dataLen).padStart(6)}B]`
  );
}

function toUint8Array(bytes) {
  if (bytes instanceof Uint8Array) return bytes;
  if (bytes instanceof ArrayBuffer) return new Uint8Array(bytes);
  if (ArrayBuffer.isView(bytes)) {
    return new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }
  throw new TypeError('packet bytes must be an ArrayBuffer or Uint8Array');
}

function createPacket(opts) {
  const { operation, reserve = 0, srcPort = 0, dstPort = 0, identifier = 0, source = 0, payload = null } = opts;

  const payloadLen = payload ? payload.length : 0;
  const total = FIXED_HEADER_LEN + payloadLen;
  const buf = new ArrayBuffer(total);
  const view = new DataView(buf);
  const u8 = new Uint8Array(buf);

  view.setUint8(0, FIXED_HEADER_LEN / 4);
  view.setUint8(1, reserve);
  view.setUint16(2, total, false);
  view.setUint32(4, operation, false);
  view.setUint16(8, srcPort, false);
  view.setUint16(10, dstPort, false);
  view.setUint32(12, identifier, false);
  view.setUint32(16, source, false);

  if (payload && payloadLen) {
    u8.set(payload, FIXED_HEADER_LEN);
  }

  return u8;
}

function parsePacket(bytes) {
  const u8 = toUint8Array(bytes);
  if (u8.byteLength < FIXED_HEADER_LEN) {
    throw new Error(`packet too short: ${u8.byteLength} bytes`);
  }

  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const headerLength = view.getUint8(0) * 4;
  const reserve = view.getUint8(1);
  const packetLength = view.getUint16(2, false);
  const operation = view.getUint32(4, false);
  const srcPort = view.getUint16(8, false);
  const dstPort = view.getUint16(10, false);
  const identifier = view.getUint32(12, false);
  const source = view.getUint32(16, false);

  if (headerLength < FIXED_HEADER_LEN) {
    throw new Error(`invalid hwlink packet header length: ${headerLength}`);
  }
  if (packetLength > u8.byteLength) {
    throw new Error(`incomplete hwlink packet: ${packetLength} > ${u8.byteLength}`);
  }

  let data = null;
  if (packetLength > FIXED_HEADER_LEN) {
    data = u8.subarray(FIXED_HEADER_LEN, packetLength);
  }

  return {
    headerLength,
    reserve,
    packetLength,
    operation,
    srcPort,
    dstPort,
    identifier,
    source,
    data,
  };
}

export {
  FIXED_HEADER_LEN,
  MAX_SEND_CHUNK_SIZE,
  OpCode,
  ReserveSource,
  createPacket,
  formatPacketOneLine,
  isOpCmdTerminalData,
  isOpCreateTcpTunnel,
  isOpFailed,
  isOpListenTcpTunnel,
  isOpReverseCreateTcpTunnel,
  isOpTcpTunnelData,
  isOpTunnelSuccess,
  isSubStreamPing,
  nextIdentifier,
  operationToString,
  parsePacket,
  toUint8Array,
};
