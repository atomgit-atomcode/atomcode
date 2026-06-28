/**
 * 语法高亮 Worker
 *
 * 在 WebWorker 中运行，接收代码文本 + 语法规则，返回语义 Token 列表。
 * 避免在主线程做正则匹配，防止大文件卡 UI。
 */

// Worker 中无法 import 外部模块，所以规则类型直接内联
interface InlineRule {
  match: string;
  capture: number;
  token: string;
  modifiers?: number;
}

interface TokenizeRequest {
  id: number;
  type: 'tokenize';
  text: string;
  rules: InlineRule[];
}

interface TokenResult {
  line: number;
  startCol: number;
  endCol: number;
  tokenType: string;
  modifiers: number;
}

interface TokenizeResponse {
  id: number;
  type: 'tokens';
  tokens: TokenResult[];
}

// 预编译规则（Worker 中只编译一次）
let compiledRules: { regex: RegExp; capture: number; token: string; modifiers: number }[] | null = null;
let lastRulesJson = '';

function ensureRules(rules: InlineRule[]) {
  const json = JSON.stringify(rules);
  if (json === lastRulesJson && compiledRules) return;
  compiledRules = rules.map((r) => ({
    regex: new RegExp(r.match, 'g'),
    capture: r.capture,
    token: r.token,
    modifiers: r.modifiers ?? 0,
  }));
  lastRulesJson = json;
}

self.onmessage = (e: MessageEvent<TokenizeRequest>) => {
  const req = e.data;
  if (req.type !== 'tokenize') return;

  ensureRules(req.rules);

  const lines = req.text.split('\n');
  const tokens: TokenResult[] = [];

  for (let lineIdx = 0; lineIdx < lines.length; lineIdx++) {
    const line = lines[lineIdx];

    for (const rule of compiledRules!) {
      // 重置正则 lastIndex
      rule.regex.lastIndex = 0;

      let m: RegExpExecArray | null;
      while ((m = rule.regex.exec(line)) !== null) {
        const captureIdx = rule.capture;
        const captureGroup = m[captureIdx];
        if (captureGroup === undefined) continue;

        const fullMatch = m[0];
        const captureIndex = m.index + (captureIdx === 0 ? 0 : (m[0].indexOf(captureGroup)));
        // ^ 上面需要更精确地计算捕获组的位置
        // 改用更可靠的方式：
        const pos = findCapturePosition(line, m.index, m, rule.capture);
        if (pos === null) continue;

        tokens.push({
          line: lineIdx,
          startCol: pos.start,
          endCol: pos.end,
          tokenType: rule.token,
          modifiers: rule.modifiers,
        });
      }
    }
  }

  // 按位置排序
  tokens.sort((a, b) => a.line - b.line || a.startCol - b.startCol);

  const resp: TokenizeResponse = { id: req.id, type: 'tokens', tokens };
  self.postMessage(resp);
};

/**
 * 精确计算捕获组在行中的起始/结束列
 */
function findCapturePosition(
  line: string,
  matchStart: number,
  matchResult: RegExpExecArray,
  captureIdx: number,
): { start: number; end: number } | null {
  if (captureIdx === 0) {
    return { start: matchStart, end: matchStart + matchResult[0].length };
  }

  const fullMatch = matchResult[0];
  const captured = matchResult[captureIdx];
  if (captured === undefined) return null;

  // 在 fullMatch 字符串中找 captured 的偏移
  let searchFrom = 0;
  for (let i = 1; i < captureIdx; i++) {
    const g = matchResult[i];
    if (g !== undefined) {
      const idx = fullMatch.indexOf(g, searchFrom);
      if (idx >= 0) searchFrom = idx + g.length;
    }
  }

  const captureOffsetInFull = fullMatch.indexOf(captured, searchFrom);
  if (captureOffsetInFull < 0) return null;

  return {
    start: matchStart + captureOffsetInFull,
    end: matchStart + captureOffsetInFull + captured.length,
  };
}
