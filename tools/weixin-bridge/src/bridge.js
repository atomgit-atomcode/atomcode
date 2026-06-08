// 编排：把入站文本转成一轮 agent 对话；权限请求转微信审批，y/n 回传 daemon。
function truncate(s, n) {
  s = s ?? '';
  return s.length > n ? s.slice(0, n) + '…' : s;
}

export function makeBridge({ sessions, agent, ilink, allowlist, workingDir }) {
  async function startTurn({ fromUserId, text, contextToken }) {
    sessions.setContextToken(fromUserId, contextToken);
    let acc = '';
    await agent.streamChat(
      { message: text, sessionId: sessions.getSessionId(fromUserId), workingDir },
      {
        onText: (c) => { acc += c; },
        onPermission: (ev) => {
          sessions.setPending(fromUserId, { sessionId: ev.session_id });
          ilink.sendMessage(
            fromUserId,
            `atomcode 想执行 ${ev.tool_name}：\n${truncate(ev.arguments, 300)}\n回复 y 同意 / n 拒绝`,
            sessions.getContextToken(fromUserId),
          );
        },
        onDone: (ev) => {
          if (ev.session_id) sessions.setSessionId(fromUserId, ev.session_id);
          ilink.sendMessage(fromUserId, acc || '(无文本输出)', sessions.getContextToken(fromUserId));
        },
        onError: (m) => {
          ilink.sendMessage(fromUserId, `出错了：${m}`, sessions.getContextToken(fromUserId));
        },
      },
    );
  }

  async function handleInbound(inbound) {
    const user = inbound.fromUserId;
    if (allowlist?.length && !allowlist.includes(user)) return;

    const pending = sessions.getPending(user);
    if (pending) {
      const decision = /^\s*y/i.test(inbound.text) ? 'allow' : 'deny';
      sessions.clearPending(user);
      sessions.setContextToken(user, inbound.contextToken);
      await agent.sendPermission(pending.sessionId, decision);
      return;
    }
    await startTurn(inbound);
  }

  return { handleInbound };
}
