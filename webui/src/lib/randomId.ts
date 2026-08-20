/**
 * 生成一个 UUID v4 风格的随机 id。
 *
 * 为什么不直接用 `crypto.randomUUID()`：该 API 只在**安全上下文**
 * (secure context) 可用 —— HTTPS，或 http://localhost / http://127.0.0.1。
 * 当 webui 以 `--host 0.0.0.0` 绑定、从局域网 IP（如 http://10.18.30.45:8081）
 * 明文 HTTP 访问时，页面不是安全上下文，`crypto.randomUUID` 为 undefined，
 * 直接调用会抛 `TypeError: crypto.randomUUID is not a function`，导致发消息路径
 * 直接崩掉（回环访问却正常，因为 127.0.0.1/localhost 被视为安全上下文）。
 *
 * `crypto.getRandomValues()` 属于基础 Crypto 接口，在非安全上下文里同样可用，
 * 用它自建 v4 即可兼顾两种场景。这些 id 仅用于请求关联/去重，不承担安全语义。
 */
export function randomId(source: Crypto | undefined = globalThis.crypto): string {
  // 安全上下文：优先原生实现。
  if (source && typeof source.randomUUID === 'function') {
    return source.randomUUID();
  }
  // 非安全上下文（局域网明文 HTTP）：randomUUID 缺失，但 getRandomValues 仍在。
  if (source && typeof source.getRandomValues === 'function') {
    const bytes = new Uint8Array(16);
    source.getRandomValues(bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
    const hex: string[] = [];
    for (const b of bytes) hex.push(b.toString(16).padStart(2, '0'));
    return (
      hex.slice(0, 4).join('') +
      '-' +
      hex.slice(4, 6).join('') +
      '-' +
      hex.slice(6, 8).join('') +
      '-' +
      hex.slice(8, 10).join('') +
      '-' +
      hex.slice(10, 16).join('')
    );
  }
  // 兜底（crypto 完全缺失的老旧环境）：非加密强度，对请求关联 id 足够。
  let out = '';
  for (let i = 0; i < 32; i++) {
    if (i === 8 || i === 12 || i === 16 || i === 20) out += '-';
    if (i === 12) out += '4';
    else if (i === 16) out += ((Math.floor(Math.random() * 4) & 0x3) | 0x8).toString(16);
    else out += Math.floor(Math.random() * 16).toString(16);
  }
  return out;
}
