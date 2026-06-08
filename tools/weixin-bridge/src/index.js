import qrcode from 'qrcode-terminal';
import { loadConfig } from './config.js';
import { IlinkClient } from './ilink.js';
import { AgentClient } from './agent.js';
import { parseInbound } from './parse.js';
import { SessionStore } from './sessions.js';
import { makeBridge } from './bridge.js';
import { loadToken, saveToken, login } from './auth.js';

const cfg = loadConfig();
const ilink = new IlinkClient({ baseUrl: cfg.ilinkBaseUrl, channelVersion: cfg.channelVersion });
const agent = new AgentClient({ baseUrl: cfg.daemonBaseUrl });
const sessions = new SessionStore();
const { handleInbound } = makeBridge({
  sessions, agent, ilink, allowlist: cfg.allowlist, workingDir: cfg.workingDir,
});

let token = loadToken(cfg.tokenPath);
if (!token) {
  console.log('未找到 token，开始扫码登录…');
  token = await login(ilink, { render: (s) => qrcode.generate(s, { small: true }) });
  saveToken(cfg.tokenPath, token);
  console.log('登录成功，token 已保存。');
}
ilink.setToken(token);

console.log(`桥已启动。daemon=${cfg.daemonBaseUrl} workingDir=${cfg.workingDir}`);
if (!cfg.allowlist?.length) console.warn('⚠ allowlist 为空：将响应所有用户。建议填入你的 from_user_id。');

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
    // 不 await：审批期间该 turn 的 SSE 挂起，循环须继续以收到 y/n。
    handleInbound(inbound).catch((e) => console.error('handleInbound 错误：', e));
  }
}
