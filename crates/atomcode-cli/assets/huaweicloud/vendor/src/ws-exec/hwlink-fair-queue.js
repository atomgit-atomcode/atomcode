/*
 * This file contains code derived from hwlink.
 *
 * Source:
 * https://gitcode.com/huawei-developers/hwlink
 *
 * Licensed under the ISC License.
 */

const DEFAULT_HIGH_WATERMARK = 8 * 1024 * 1024;
const DEFAULT_LOW_WATERMARK = 2 * 1024 * 1024;

class FairQueue {
  constructor(onSend, opts = {}) {
    this.onSend = onSend;
    this.channels = new Map();
    this.readyChannels = new Set();
    this.channelOrder = [];
    this.cursor = 0;
    this.normalQueue = [];
    this.highPriorityQueue = [];
    this.draining = false;
    this.totalBytes = 0;
    this.backpressureQueue = [];
    this.highWatermark = opts.highWatermark || DEFAULT_HIGH_WATERMARK;
    this.lowWatermark = opts.lowWatermark || DEFAULT_LOW_WATERMARK;
  }

  register(ch) {
    this.channels.set(ch.identifier, []);
  }

  unregister(ch) {
    this.channels.delete(ch.identifier);
    this.readyChannels.delete(ch.identifier);
  }

  async sendFairly(ch, data) {
    if (this.totalBytes >= this.highWatermark) {
      await new Promise((resolve) => {
        this.backpressureQueue.push(resolve);
      });
    }

    const buffer = this.channels.get(ch.identifier);
    if (!buffer) return;

    buffer.push(data);
    this.totalBytes += data.byteLength;
    if (!this.readyChannels.has(ch.identifier)) {
      this.readyChannels.add(ch.identifier);
      this.channelOrder.push(ch.identifier);
    }
    this.startDrain();
  }

  sendImmediately(data) {
    this.highPriorityQueue.push(data);
    this.totalBytes += data.byteLength;
    this.startDrain();
  }

  startDrain() {
    if (this.draining) return;
    this.draining = true;
    this.drain().catch(() => {});
  }

  async drain() {
    try {
      while (true) {
        if (this.highPriorityQueue.length > 0) {
          const data = this.highPriorityQueue.shift();
          this.totalBytes -= data.byteLength;
          await this.onSend(data);
          this.checkBackpressure();
          continue;
        }

        if (this.normalQueue.length > 0) {
          const data = this.normalQueue.shift();
          this.totalBytes -= data.byteLength;
          await this.onSend(data);
          this.checkBackpressure();
          continue;
        }

        this.collectRound();

        if (this.highPriorityQueue.length === 0 && this.normalQueue.length === 0) {
          break;
        }
      }
    } finally {
      this.draining = false;
    }
  }

  collectRound() {
    if (this.readyChannels.size === 0) return;
    const len = this.channelOrder.length;
    if (len === 0) return;

    this.cursor %= len;
    const start = this.cursor;

    for (let i = 0; i < len; i += 1) {
      const idx = (start + i) % len;
      const id = this.channelOrder[idx];
      if (!this.readyChannels.has(id)) continue;

      const buffer = this.channels.get(id);
      if (!buffer || buffer.length === 0) {
        this.readyChannels.delete(id);
        continue;
      }

      const data = buffer.shift();
      this.normalQueue.push(data);
      if (buffer.length === 0) {
        this.readyChannels.delete(id);
      }
    }

    this.cursor = (start + len) % len;
    this.channelOrder = Array.from(new Set(this.channelOrder.filter((id) => this.readyChannels.has(id))));
  }

  checkBackpressure() {
    while (this.totalBytes < this.lowWatermark && this.backpressureQueue.length > 0) {
      const resolve = this.backpressureQueue.shift();
      resolve();
    }
  }
}

export { DEFAULT_HIGH_WATERMARK, DEFAULT_LOW_WATERMARK, FairQueue };
