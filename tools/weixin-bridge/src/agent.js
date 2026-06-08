// atomcode daemon 客户端：POST /chat (SSE 流) + POST /chat/permission。

// 从累积缓冲里切出完整 SSE 事件（"data: {json}\n\n"），返回事件数组与残块。
export function parseSseLines(buffer) {
  const parts = buffer.split('\n\n');
  const rest = parts.pop() ?? ''; // 末段可能不完整，留到下次；恰好以 \n\n 结尾时为空串而非 undefined
  const events = [];
  for (const block of parts) {
    for (const line of block.split('\n')) {
      const m = line.match(/^data:\s?(.*)$/);
      if (!m) continue;
      try { events.push(JSON.parse(m[1])); } catch { /* 跳过非 JSON 行 */ }
    }
  }
  return { events, rest };
}

// 把一个 ChatEvent 派发到 handlers（未知类型忽略）。
export function dispatch(ev, h) {
  switch (ev.type) {
    case 'text': h.onText?.(ev.content); break;
    case 'permission_request': h.onPermission?.(ev); break;
    case 'done': h.onDone?.(ev); break;
    case 'error': h.onError?.(ev.message); break;
    default: break;
  }
}

export class AgentClient {
  constructor({ baseUrl, fetchImpl } = {}) {
    this.baseUrl = baseUrl;
    this.fetch = fetchImpl || globalThis.fetch;
  }

  // 发起一轮对话，SSE 事件逐个派发给 handlers。
  // 注意：当 agent 需要审批时，daemon 会 hold 住流直到 /chat/permission 到达，
  // 因此本调用在审批期间不会 resolve——调用方不应让它阻塞入站长轮询循环。
  async streamChat({ message, sessionId, workingDir }, handlers) {
    const res = await this.fetch(`${this.baseUrl}/chat`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ message, session_id: sessionId, working_dir: workingDir }),
    });
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const { events, rest } = parseSseLines(buf);
      buf = rest;
      for (const ev of events) dispatch(ev, handlers);
    }
  }

  async sendPermission(sessionId, decision) {
    const res = await this.fetch(`${this.baseUrl}/chat/permission`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ session_id: sessionId, decision }),
    });
    return res.json();
  }
}
