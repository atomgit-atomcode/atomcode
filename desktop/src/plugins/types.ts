/**
 * Plugin 系统的类型定义
 *
 * 插件格式（TextMate Grammar 风格）：
 *   ~/.atomcode/plugins/<name>/
 *     package.json     — 元数据
 *     grammar.json     — 高亮规则
 */

// ============================================================================
// 插件元数据
// ============================================================================

export interface PluginPackage {
  /** 唯一标识，如 "ts-extra" */
  name: string;
  /** 显示名，如 "TypeScript Extra Highlight" */
  display: string;
  /** 版本号 */
  version: string;
  /** 作者 */
  author?: string;
  /** 描述 */
  description?: string;
  /** 适用的 Monaco 语言 ID，如 ["typescript", "javascript"] */
  languages: string[];
  /** 是否随应用内置（不可卸载，但可禁用） */
  builtin?: boolean;
  /** 安装日期（时间戳） */
  installedAt?: number;
  /** 是否被禁用（持久化在 ~/.atomcode/plugins/config.json） */
  disabled?: boolean;
}

// ============================================================================
// 语法高亮规则
// ============================================================================

/**
 * 单条高亮规则
 *
 * 示例：匹配函数名
 *   { "match": "\\b([a-zA-Z_$][\\w$]*)\\s*(?=\\()", "capture": 1, "token": "function" }
 */
export interface GrammarRule {
  /** 正则表达式（不含 / / 分隔符），匹配整个结构或捕获组 */
  match: string;
  /** 取哪个捕获组作为高亮位置（1-based；0 = 整个匹配） */
  capture: number;
  /** 对应 LEGEND.tokenTypes 中的类型名 */
  token: string;
  /** 修饰符（可选），对应 LEGEND.tokenModifiers */
  modifiers?: string[];
}

/**
 * 语法文件结构
 */
export interface GrammarFile {
  /** 适用的 Monaco 语言 ID */
  scope: string;
  rules: GrammarRule[];
}

// ============================================================================
// 语义 Token 图例（与 Monaco 约定一致）
// ============================================================================

/**
 * Monaco 的 DocumentSemanticTokensProvider 要求定义 tokenTypes 和 tokenModifiers 的 legend。
 * 这些是所有插件共享的 Token 类型枚举。
 */
export const SEMANTIC_TOKEN_LEGEND = {
  tokenTypes: [
    'function',
    'method',
    'class',
    'interface',
    'variable',
    'parameter',
    'property',
    'enumMember',
    'type',
    'namespace',
    'decorator',
  ] as const,
  tokenModifiers: [
    'declaration',
    'readonly',
    'static',
    'async',
    'abstract',
    'local',
  ] as const,
};

/** 语义 Token 类型索引（name → index） */
export const TOKEN_TYPE_MAP: Record<string, number> = {};
SEMANTIC_TOKEN_LEGEND.tokenTypes.forEach((t, i) => { TOKEN_TYPE_MAP[t] = i; });

/** 语义 Token 修饰符位掩码 */
export const TOKEN_MODIFIER_MAP: Record<string, number> = {};
SEMANTIC_TOKEN_LEGEND.tokenModifiers.forEach((m, i) => { TOKEN_MODIFIER_MAP[m] = 1 << i; });

/** TokenType 的联合字符串类型 */
export type TokenType = typeof SEMANTIC_TOKEN_LEGEND.tokenTypes[number];
export type TokenModifier = typeof SEMANTIC_TOKEN_LEGEND.tokenModifiers[number];

// ============================================================================
// WebWorker 消息协议
// ============================================================================

/** 发送给 Worker 的消息 */
export interface WorkerRequest {
  id: number;
  type: 'tokenize';
  /** 代码文本 */
  text: string;
  /** 语法规则 */
  rules: GrammarRule[];
}

/** Worker 返回的消息 */
export interface WorkerResponse {
  id: number;
  type: 'tokens';
  /** 按行组织的 Token 列表 [{ line, startCol, endCol, tokenType, modifiers }] */
  tokens: WorkerToken[];
}

export interface WorkerToken {
  /** 0-based 行号 */
  line: number;
  /** 0-based 列（UTF-16 code unit） */
  startCol: number;
  endCol: number;
  tokenType: string;
  modifiers: number;
}
