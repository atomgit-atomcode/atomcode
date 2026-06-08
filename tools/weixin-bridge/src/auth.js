import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { dirname } from 'node:path';

export function loadToken(path) {
  if (!existsSync(path)) return null;
  try {
    const data = JSON.parse(readFileSync(path, 'utf8'));
    return data.botToken ?? null;
  } catch {
    return null;
  }
}

export function saveToken(path, botToken) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify({ botToken }, null, 2), 'utf8');
}

// 扫码登录：取二维码 -> 渲染 -> 轮询状态 -> 返回 botToken。
// render(qrcodeStr) 由调用方提供（终端用 qrcode-terminal）。
export async function login(ilink, { render, pollIntervalMs = 1500, maxWaitMs = 180000 } = {}) {
  const qr = await ilink.getBotQrcode();
  const qrcode = qr.qrcode ?? qr.url;
  if (!qrcode) throw new Error(`get_bot_qrcode 未返回 qrcode 字段: ${JSON.stringify(qr)}`);
  render(qrcode);

  const deadline = Date.now() + maxWaitMs;
  for (;;) {
    if (Date.now() > deadline) throw new Error('扫码超时');
    const st = await ilink.getQrcodeStatus(qrcode);
    if (st.status === 'confirmed' && st.bot_token) {
      if (st.baseurl) ilink.baseUrl = st.baseurl; // 登录后服务器可能指定专属 baseurl
      return st.bot_token;
    }
    await new Promise((r) => setTimeout(r, pollIntervalMs));
  }
}
