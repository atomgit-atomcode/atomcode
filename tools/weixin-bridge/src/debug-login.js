// 一次性诊断脚本（非功能代码）：把 iLink 登录两个边界的真实响应原样打印出来，
// 用于核对我们逆向假设的字段名/状态值是否正确。长字符串(疑似 token)自动脱敏，
// 输出可安全粘贴。用法：node src/debug-login.js  然后扫码。
import qrcode from 'qrcode-terminal';
import { loadConfig } from './config.js';
import { IlinkClient } from './ilink.js';

// 递归脱敏：>12 长度的字符串替换为 <masked str len=N>，保留短字符串/数字/布尔(如 status 值)。
function mask(v) {
  if (typeof v === 'string') return v.length > 12 ? `<masked str len=${v.length}>` : v;
  if (Array.isArray(v)) return v.map(mask);
  if (v && typeof v === 'object') {
    const o = {};
    for (const k of Object.keys(v)) o[k] = mask(v[k]);
    return o;
  }
  return v;
}

const cfg = loadConfig();
const ilink = new IlinkClient({ baseUrl: cfg.ilinkBaseUrl, channelVersion: cfg.channelVersion });

console.log('=== 1) get_bot_qrcode 原始响应(脱敏) ===');
const qr = await ilink.getBotQrcode();
console.log(JSON.stringify(mask(qr), null, 2));
console.log('=== 顶层字段名:', Object.keys(qr).join(', '));

// 渲染 qrcode_img_content(授权 URL)，轮询用 qrcode(id)——二者不同。
const scanContent = qr.qrcode_img_content ?? qr.url ?? qr.qrcode;
const pollId = qr.qrcode;
console.log('=== 渲染字段 qrcode_img_content(脱敏):', mask(scanContent), '| 轮询字段 qrcode(脱敏):', mask(pollId));

if (scanContent) {
  qrcode.generate(scanContent, { small: true });
} else {
  console.log('⚠ 无 qrcode_img_content/url，无法渲染——见上面字段名。');
}

console.log('\n=== 2) 请用微信扫码。下面逐次打印 get_qrcode_status 原始响应(脱敏) ===');
console.log('(扫码并在手机上确认后，注意哪一次响应发生了变化、status 字段叫什么/值是什么、token 落在哪个字段)\n');

for (let i = 0; i < 40; i++) {
  try {
    const st = await ilink.getQrcodeStatus(pollId);
    console.log(`[poll ${i}]`, JSON.stringify(mask(st)));
  } catch (e) {
    console.log(`[poll ${i}] ERROR:`, e.message);
  }
  await new Promise((r) => setTimeout(r, 1500));
}
console.log('\n诊断结束（仅打印，不保存任何东西）。把上面整段输出贴回来。');
