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
// render(scanContent) 由调用方提供（终端用 qrcode-terminal）。
// get_bot_qrcode 返回两个不同字段，务必分清（实测，2026-06）：
//   qrcode             — 轮询用的 session id（get_qrcode_status?qrcode=<id>）
//   qrcode_img_content — 微信能识别的授权内容(URL)，这才是要编码进二维码的内容
// 若误把 qrcode 渲染成二维码，微信只会显示「扫描结果: <id>」纯文本，无授权页，
// status 永远停在 "wait"。
export async function login(ilink, { render, pollIntervalMs = 1500, maxWaitMs = 180000 } = {}) {
  const qr = await ilink.getBotQrcode();
  const scanContent = qr.qrcode_img_content ?? qr.url ?? qr.qrcode; // 授权 URL
  const pollId = qr.qrcode;                                          // 轮询 id
  if (!scanContent || !pollId) {
    throw new Error(`get_bot_qrcode 字段缺失，实际字段: ${JSON.stringify(Object.keys(qr))}`);
  }
  render(scanContent);

  const deadline = Date.now() + maxWaitMs;
  for (;;) {
    if (Date.now() > deadline) throw new Error('扫码超时');
    const st = await ilink.getQrcodeStatus(pollId);
    if (st.status === 'confirmed' && st.bot_token) {
      if (st.baseurl) ilink.baseUrl = st.baseurl; // 登录后服务器可能指定专属 baseurl
      return st.bot_token;
    }
    await new Promise((r) => setTimeout(r, pollIntervalMs));
  }
}
