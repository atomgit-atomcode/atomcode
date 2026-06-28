/**
 * 插件管理器
 *
 * 负责：
 *   1. 通过 Electron IPC 列出、安装、卸载、启用/禁用插件
 *   2. 加载启用的插件的 grammar 并注册到语法加载器
 *   3. 内置插件（如 ts-extra）也可被禁用
 */

import type { PluginPackage, GrammarFile, GrammarRule } from './types';
import { addGrammarRules, refreshGrammarProviders } from './grammarLoader';

// ============================================================================
// 内置插件（随应用打包）
// ============================================================================

/**
 * 内置的 TypeScript / JavaScript 附加高亮规则
 *
 * 设计目标：补齐 Monaco 原生高亮之外的"语义级"高亮——
 * 函数名、类名、接口、类型别名、变量、参数、属性、枚举、装饰器等。
 *
 * 规则按优先级排列：先匹配 declaration（定义处），再匹配 reference（使用处），
 * 避免 reference 把 declaration 的捕获组吃掉。
 */
const TS_GRAMMAR_RULES: GrammarRule[] = [
  // ─── 函数定义 ─────────────────────────────────────────────────────────
  // function foo(  →  函数名
  { match: '\\bfunction\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'function', modifiers: ['declaration'] },
  // const foo = (args) =>  或  const foo = function (...)  →  函数名
  { match: '\\b(?:const|let|var)\\s+([a-zA-Z_$][\\w$]*)\\s*=\\s*(?:async\\s*)?(?:\\([^)]*\\)|function)\\s*=>?', capture: 1, token: 'function', modifiers: ['declaration'] },
  // class Foo { method(  →  方法名
  { match: '\\b([a-zA-Z_$][\\w$]*)\\s*(?=\\([^)]*\\)\\s*\\{)', capture: 1, token: 'method', modifiers: ['declaration'] },

  // ─── 函数调用（reference） ───────────────────────────────────────────
  // foo(  或  obj.foo(  →  函数名
  { match: '(?:\\.|\\?\\.)?\\s*([a-zA-Z_$][\\w$]*)\\s*(?=\\()', capture: 1, token: 'function' },

  // ─── 类 / 接口 / 类型别名 ────────────────────────────────────────────
  // class Foo / interface Foo / type Foo  →  类型名
  { match: '\\b(?:class|interface|type|enum)\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'class', modifiers: ['declaration'] },
  // extends Foo / implements Foo  →  类型名
  { match: '\\bextends\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'class' },
  { match: '\\bimplements\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'interface' },
  // new Foo(  →  类名
  { match: '\\bnew\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'class' },

  // ─── 类型注解（变量: Type） ─────────────────────────────────────────
  // : string / : number / ...（关键字类型）
  { match: ':\\s*(string|number|boolean|void|null|undefined|any|never|unknown|bigint|symbol|object)\\b', capture: 1, token: 'type' },
  // : Foo（大写开头的类型引用，避免误伤小写变量）
  { match: ':\\s*([A-Z][\\w$]*)', capture: 1, token: 'type' },
  // Array<X> / Promise<X> 等泛型类型
  { match: ':\\s*([A-Z][\\w$]*)\\s*<', capture: 1, token: 'type' },

  // ─── 变量声明 ─────────────────────────────────────────────────────────
  // const x / let y / var z  →  变量名（非函数）
  { match: '\\b(?:const|let|var)\\s+([a-zA-Z_$][\\w$]*)\\s*(?:[=:;]|$)', capture: 1, token: 'variable', modifiers: ['declaration', 'readonly'] },

  // ─── 函数参数 ─────────────────────────────────────────────────────────
  // (param: Type) 或 (param, ...) →  参数名
  { match: '\\(\\s*([a-zA-Z_$][\\w$]*)\\s*(?=[,:)])', capture: 1, token: 'parameter', modifiers: ['declaration'] },
  { match: ',\\s*([a-zA-Z_$][\\w$]*)\\s*(?=[,:)])', capture: 1, token: 'parameter', modifiers: ['declaration'] },

  // ─── 属性名（对象字面量 / 解构） ──────────────────────────────────────
  // { key: value }  →  key
  { match: '\\{\\s*([a-zA-Z_$][\\w$]*)\\s*:', capture: 1, token: 'property' },
  // { key1, key2 }  解构
  { match: '\\{\\s*([a-zA-Z_$][\\w$]*)\\s*(?:,|\\})', capture: 1, token: 'property' },
  // obj.prop  →  prop
  { match: '(?:\\.|\\?\\.)\\s*([a-zA-Z_$][\\w$]*)', capture: 1, token: 'property' },

  // ─── 命名空间 / 模块 ─────────────────────────────────────────────────
  // namespace Foo / module Foo
  { match: '\\b(?:namespace|module)\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'namespace', modifiers: ['declaration'] },

  // ─── 装饰器 ──────────────────────────────────────────────────────────
  // @Component / @Injectable
  { match: '(@[a-zA-Z_$][\\w$]*)', capture: 1, token: 'decorator' },

  // ─── 常量大写变量（约定） ─────────────────────────────────────────────
  // const UPPER_CASE = ...  →  变量名当作 readonly
  { match: '\\b(?:const|let|var)\\s+([A-Z][A-Z0-9_]+)\\b', capture: 1, token: 'variable', modifiers: ['readonly', 'static'] },
];

const BUILTIN_TYPESCRIPT_GRAMMAR: GrammarFile = {
  scope: 'typescript',
  rules: TS_GRAMMAR_RULES,
};

const BUILTIN_JAVASCRIPT_GRAMMAR: GrammarFile = {
  scope: 'javascript',
  rules: TS_GRAMMAR_RULES.map((r) => ({ ...r })),
};

/** 内置插件定义 */
export const BUILTIN_PLUGINS: PluginPackage[] = [
  {
    name: 'ts-extra',
    display: 'TypeScript Extra Highlight',
    version: '1.1.0',
    description: 'Additional syntax highlighting for TypeScript/JavaScript: function names, classes, interfaces, type annotations, variables, parameters, properties, namespaces, decorators.',
    author: 'AtomCode',
    languages: ['typescript', 'javascript'],
    builtin: true,
  },
];

/** 内置插件名 → grammar 映射 */
const BUILTIN_GRAMMARS: Record<string, GrammarFile> = {
  'ts-extra': BUILTIN_TYPESCRIPT_GRAMMAR,
};
// 同一份规则也注册到 javascript
const BUILTIN_GRAMMARS_BY_LANG: Record<string, GrammarFile> = {
  'ts-extra': BUILTIN_JAVASCRIPT_GRAMMAR,
};

// ============================================================================
// 状态
// ============================================================================

let installedPlugins: PluginPackage[] = [];
let initialized = false;
/** 已禁用的插件名集合（含内置插件） */
let disabledPlugins = new Set<string>();

// ============================================================================
// IPC 封装
// ============================================================================

export interface PluginIpc {
  listPlugins: () => Promise<PluginPackage[]>;
  installPlugin: () => Promise<{ success: boolean; plugin?: PluginPackage; error?: string }>;
  uninstallPlugin: (name: string) => Promise<{ success: boolean; error?: string }>;
  readGrammar: (name: string) => Promise<GrammarFile | null>;
  isDisabled: (name: string) => Promise<boolean>;
  setDisabled: (name: string, disabled: boolean) => Promise<{ success: boolean }>;
}

function getIpc(): PluginIpc | null {
  const api = (window as any).electronAPI;
  if (!api || !api.plugins) return null;
  return api.plugins;
}

// ============================================================================
// 公开 API
// ============================================================================

/**
 * 初始化插件系统：注册内置插件 + 加载已安装插件
 *
 * 注意：内置插件如果被禁用则不注册其 grammar。
 */
export async function initializePlugins(): Promise<void> {
  if (initialized) return;
  initialized = true;

  const ipc = getIpc();

  // 1. 查询每个内置插件的禁用状态
  for (const pkg of BUILTIN_PLUGINS) {
    let disabled = false;
    if (ipc) {
      try { disabled = await ipc.isDisabled(pkg.name); } catch {}
    }
    if (!disabled) {
      registerBuiltinGrammar(pkg.name);
      disabledPlugins.delete(pkg.name);
    } else {
      disabledPlugins.add(pkg.name);
    }
  }

  // 2. 加载已安装插件
  if (ipc) {
    try {
      const plugins = await ipc.listPlugins();
      installedPlugins = plugins;
      for (const plugin of plugins) {
        if (plugin.disabled) {
          disabledPlugins.add(plugin.name);
          continue;
        }
        const grammar = await ipc.readGrammar(plugin.name);
        if (grammar) {
          addGrammarRules(grammar.scope, grammar.rules);
        }
      }
    } catch (err) {
      console.error('[pluginManager] Failed to load installed plugins:', err);
    }
  }

  await refreshGrammarProviders();
  console.debug(`[pluginManager] Initialized: ${installedPlugins.length} installed + ${BUILTIN_PLUGINS.length} builtin (${disabledPlugins.size} disabled)`);
}

function registerBuiltinGrammar(name: string): void {
  const grammar = BUILTIN_GRAMMARS[name];
  if (grammar) addGrammarRules(grammar.scope, grammar.rules);
  const byLang = BUILTIN_GRAMMARS_BY_LANG[name];
  if (byLang) addGrammarRules(byLang.scope, byLang.rules);
}

function unregisterBuiltinGrammar(name: string): void {
  // grammarLoader 暂时不支持按插件移除规则；禁用=不再注册即可
  // 由于初始化时已经 addGrammarRules，禁用后需要刷新编辑器重新 tokenize
  // 简化方案：禁用后只是阻止后续注册，已注册的规则保留（重启后生效）
  // 见 setPluginEnabled
}

/**
 * 获取所有插件（内置 + 已安装），带禁用状态
 */
export function getAllPlugins(): PluginPackage[] {
  return [
    ...BUILTIN_PLUGINS.map((p) => ({ ...p, disabled: disabledPlugins.has(p.name) })),
    ...installedPlugins.map((p) => ({ ...p, disabled: disabledPlugins.has(p.name) })),
  ];
}

/**
 * 安装插件（打开文件对话框）
 */
export async function installPlugin(): Promise<{ success: boolean; plugin?: PluginPackage; error?: string }> {
  const ipc = getIpc();
  if (!ipc) return { success: false, error: 'Not running in Electron' };

  const result = await ipc.installPlugin();
  if (result.success && result.plugin) {
    installedPlugins.push(result.plugin!);
    const grammar = await ipc.readGrammar(result.plugin.name);
    if (grammar) {
      addGrammarRules(grammar.scope, grammar.rules);
      await refreshGrammarProviders();
    }
  }
  return result;
}

/**
 * 卸载插件（仅限已安装插件，不能卸载内置）
 */
export async function uninstallPlugin(name: string): Promise<{ success: boolean; error?: string }> {
  if (BUILTIN_PLUGINS.some((p) => p.name === name)) {
    return { success: false, error: 'Cannot uninstall built-in plugin (use disable instead)' };
  }
  const ipc = getIpc();
  if (!ipc) return { success: false, error: 'Not running in Electron' };

  const result = await ipc.uninstallPlugin(name);
  if (result.success) {
    installedPlugins = installedPlugins.filter((p) => p.name !== name);
    disabledPlugins.delete(name);
  }
  return result;
}

/**
 * 启用/禁用插件（含内置插件）
 *
 * 注意：禁用状态下插件 grammar 不被注册（重启后生效）。
 * 当前会话中已注册的规则不会移除，但禁用状态会持久化。
 */
export async function setPluginEnabled(name: string, enabled: boolean): Promise<{ success: boolean; error?: string }> {
  const ipc = getIpc();
  if (!ipc) return { success: false, error: 'Not running in Electron' };

  try {
    await ipc.setDisabled(name, !enabled);
    if (enabled) {
      disabledPlugins.delete(name);
      // 如果是内置插件，立即注册 grammar
      if (BUILTIN_PLUGINS.some((p) => p.name === name)) {
        registerBuiltinGrammar(name);
        await refreshGrammarProviders();
      } else {
        // 已安装插件：从 IPC 读取 grammar 并注册
        const grammar = await ipc.readGrammar(name);
        if (grammar) {
          addGrammarRules(grammar.scope, grammar.rules);
          await refreshGrammarProviders();
        }
      }
    } else {
      disabledPlugins.add(name);
      // 注意：不主动移除已注册的规则，重启后生效
    }
    return { success: true };
  } catch (err: any) {
    return { success: false, error: err.message };
  }
}
