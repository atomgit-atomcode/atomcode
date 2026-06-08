import qrcode from 'qrcode-terminal';
import { loadConfig } from './config.js';
import { IlinkClient } from './ilink.js';
import { parseInbound } from './parse.js';
import { loadToken, saveToken, login } from './auth.js';

const cfg = loadConfig();
const ilink = new IlinkClient({ baseUrl: cfg.ilinkBaseUrl, channelVersion: cfg.channelVersion });

let token = loadToken(cfg.tokenPath);
if (!token) {
  console.log('未找到 token，开始扫码登录…');
  token = await login(ilink, { render: (s) => qrcode.generate(s, { small: true }) });
  saveToken(cfg.tokenPath, token);
  console.log('登录成功，token 已保存。');
}
ilink.setToken(token);

console.log('开始长轮询，给 bot 发消息试试（Ctrl+C 退出）…');
let buf = '';
for (;;) {
  let resp;
  try {
    resp = await ilink.getUpdates(buf);
  } catch (e) {
    console.error('getUpdates 失败，2s 后重试：', e.message);
    await new Promise((r) => setTimeout(r, 2000));
    continue;
  }
  if (resp.get_updates_buf) buf = resp.get_updates_buf;
  for (const msg of resp.msgs ?? []) {
    const inbound = parseInbound(msg);
    if (!inbound) continue;
    console.log(`[inbound] from_user_id=${inbound.fromUserId} text=${inbound.text}`);
    await ilink.sendMessage(inbound.fromUserId, `echo: ${inbound.text}`, inbound.contextToken);
  }
}
