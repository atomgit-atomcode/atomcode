import crypto from 'node:crypto';

// X-WECHAT-UIN: 随机 uint32 -> 十进制字符串 -> base64，每次请求都变，防重放。
export function randomUin() {
  const n = crypto.randomInt(0, 0x100000000); // [0, 2^32)
  return Buffer.from(String(n), 'utf8').toString('base64');
}

export function buildHeaders(botToken) {
  const h = {
    'Content-Type': 'application/json',
    'AuthorizationType': 'ilink_bot_token',
    'X-WECHAT-UIN': randomUin(),
  };
  if (botToken) h['Authorization'] = `Bearer ${botToken}`;
  return h;
}

export class IlinkClient {
  constructor({ baseUrl, fetchImpl, channelVersion = '1.0.2' } = {}) {
    this.baseUrl = baseUrl;
    this.fetch = fetchImpl || globalThis.fetch;
    this.channelVersion = channelVersion;
    this.token = null;
  }
  setToken(t) { this.token = t; }

  async _get(path) {
    const res = await this.fetch(`${this.baseUrl}/${path}`, { headers: buildHeaders(this.token) });
    return res.json();
  }
  async _post(path, body) {
    const res = await this.fetch(`${this.baseUrl}/${path}`, {
      method: 'POST',
      headers: buildHeaders(this.token),
      body: JSON.stringify(body),
    });
    return res.json();
  }

  getBotQrcode() { return this._get('ilink/bot/get_bot_qrcode?bot_type=3'); }
  getQrcodeStatus(qrcode) {
    return this._get(`ilink/bot/get_qrcode_status?qrcode=${encodeURIComponent(qrcode)}`);
  }
  getUpdates(buf) {
    return this._post('ilink/bot/getupdates', {
      get_updates_buf: buf ?? '',
      base_info: { channel_version: this.channelVersion },
    });
  }
  sendMessage(toUserId, text, contextToken) {
    return this._post('ilink/bot/sendmessage', {
      msg: {
        to_user_id: toUserId,
        message_type: 2,
        message_state: 2,
        context_token: contextToken,
        item_list: [{ type: 1, text_item: { text } }],
      },
    });
  }
}
