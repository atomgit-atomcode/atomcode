/**
 * 语法高亮加载器
 *
 * 管理语法规则 Worker，通过 Monaco 的 registerDocumentSemanticTokensProvider
 * 为各语言注册语义 Token 提供者，将额外的高亮叠加在内置语法高亮之上。
 */

import type { editor as monacoEditor } from 'monaco-editor';
import { loader } from '@monaco-editor/react';
import type {
  GrammarRule,
  WorkerRequest,
  WorkerResponse,
  WorkerToken as WorkerTokenType,
} from './types';
import {
  SEMANTIC_TOKEN_LEGEND,
  TOKEN_TYPE_MAP,
  TOKEN_MODIFIER_MAP,
} from './types';

// ============================================================================
// Worker 管理
// ============================================================================

let worker: Worker | null = null;
let tokenIdCounter = 0;
const pendingRequests = new Map<number, { resolve: (tokens: WorkerTokenType[]) => void; reject: (err: any) => void }>();

function getWorker(): Worker {
  if (!worker) {
    worker = new Worker(new URL('./grammarWorker.ts', import.meta.url), { type: 'module' });
    worker.onmessage = (e: MessageEvent<WorkerResponse>) => {
      const resp = e.data;
      const pending = pendingRequests.get(resp.id);
      if (pending) {
        pendingRequests.delete(resp.id);
        if (resp.type === 'tokens') {
          pending.resolve(resp.tokens);
        }
      }
    };
    worker.onerror = (err) => {
      console.error('[grammarWorker] error:', err);
    };
  }
  return worker;
}

function requestTokenize(text: string, rules: GrammarRule[]): Promise<WorkerTokenType[]> {
  return new Promise((resolve, reject) => {
    const id = ++tokenIdCounter;
    pendingRequests.set(id, { resolve, reject });
    const req: WorkerRequest = { id, type: 'tokenize', text, rules };
    getWorker().postMessage(req);
  });
}

// ============================================================================
// Language → Rules 注册表
// ============================================================================

/** key: Monaco language ID, value: rules[] */
const languageRules = new Map<string, GrammarRule[]>();

/**
 * 为某语言添加语法规则
 */
export function addGrammarRules(language: string, rules: GrammarRule[]): void {
  const existing = languageRules.get(language) ?? [];
  existing.push(...rules);
  languageRules.set(language, existing);
}

/**
 * 获取某语言的全部规则
 */
export function getGrammarRules(language: string): GrammarRule[] {
  return languageRules.get(language) ?? [];
}

// ============================================================================
// Monaco Semantic Tokens Provider
// ============================================================================

let providersRegistered = new Set<string>();

/**
 * 为指定语言注册语义 Token 提供者（如果尚未注册）
 */
async function ensureProvider(language: string): Promise<void> {
  if (providersRegistered.has(language)) return;
  providersRegistered.add(language);

  const monaco = await loader.init();

  monaco.languages.registerDocumentSemanticTokensProvider(language, {
    getLegend: () => ({
      tokenTypes: SEMANTIC_TOKEN_LEGEND.tokenTypes as unknown as string[],
      tokenModifiers: SEMANTIC_TOKEN_LEGEND.tokenModifiers as unknown as string[],
    }),

    provideDocumentSemanticTokens: async (model: any, _lastTokenId: number, _token: any) => {
      const rules = languageRules.get(language);
      if (!rules || rules.length === 0) {
        return { data: new Uint32Array(0) };
      }

      const text = model.getValue();
      const workerTokens = await requestTokenize(text, rules);

      // 编码为 Uint32Array: [lineDelta, deltaStart, length, tokenType, tokenModifiers]
      const data = new Uint32Array(workerTokens.length * 5);
      let prevLine = 0;
      let prevStart = 0;

      for (let i = 0; i < workerTokens.length; i++) {
        const t = workerTokens[i];
        const lineDelta = t.line - prevLine;
        const deltaStart = lineDelta === 0 ? t.startCol - prevStart : t.startCol;

        const typeIdx = TOKEN_TYPE_MAP[t.tokenType] ?? 0;
        const modMask = t.modifiers;

        const offset = i * 5;
        data[offset] = lineDelta;
        data[offset + 1] = deltaStart;
        data[offset + 2] = t.endCol - t.startCol;
        data[offset + 3] = typeIdx;
        data[offset + 4] = modMask;

        prevLine = t.line;
        prevStart = t.startCol;
      }

      return { data };
    },

    releaseDocumentSemanticTokens: () => {},
  });

  console.debug(`[grammarLoader] Registered semantic tokens provider for "${language}"`);
}

/**
 * 初始化语法加载器：为所有已注册的语言注册提供者
 */
export async function initializeGrammarLoader(): Promise<void> {
  for (const lang of languageRules.keys()) {
    await ensureProvider(lang);
  }
}

/**
 * 加载插件后，为新语言注册提供者
 */
export async function refreshGrammarProviders(): Promise<void> {
  const registered = new Set(providersRegistered);
  for (const lang of languageRules.keys()) {
    if (!registered.has(lang)) {
      await ensureProvider(lang);
    }
  }
}
