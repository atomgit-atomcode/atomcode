/**
 * 插件管理器
 *
 * 负责：
 *   1. 通过 Electron IPC 列出、安装、卸载插件
 *   2. 加载插件的 grammar 并注册到语法加载器
 *   3. 缓存插件列表
 */

import type { PluginPackage, GrammarFile } from './types';
import { addGrammarRules, refreshGrammarProviders } from './grammarLoader';

// ============================================================================
// 内置插件（随应用打包）
// ============================================================================

/**
 * 内置的 TypeScript 附加高亮规则
 * 当做默认插件，无需安装
 */
const BUILTIN_TYPESCRIPT_GRAMMAR: GrammarFile = {
  scope: 'typescript',
  rules: [
    // === 函数名（函数声明 + 函数调用） ===
    // function foo( / const foo = ( / foo.bar(
    { match: '\\b(function\\s+)?([a-zA-Z_$][\\w$]*)\\s*(?=\\()', capture: 2, token: 'function' },
    // 箭头函数中的参数
    { match: '\\(\\s*([a-zA-Z_$][\\w$]*)\\s*(?:[,)])', capture: 1, token: 'parameter' },

    // === 类 / 接口 / 类型 ===
    { match: '\\b(class|interface|type)\\s+([a-zA-Z_$][\\w$]*)', capture: 2, token: 'class' },
    { match: '\\bextends\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'class' },
    { match: '\\bimplements\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'interface' },

    // === 类型注解（: string / : number 等） ===
    { match: ':\\s*(string|number|boolean|void|null|undefined|any|never|unknown|bigint|symbol)', capture: 1, token: 'type' },
    // 泛型类型
    { match: ':\\s*([A-Z][\\w$]*)', capture: 1, token: 'type' },

    // === 变量声明 ===
    { match: '\\b(const|let|var)\\s+([a-zA-Z_$][\\w$]*)', capture: 2, token: 'variable' },

    // === 属性名（对象字面量 / 类属性）===
    { match: '\\b([a-zA-Z_$][\\w$]*)\\s*:', capture: 1, token: 'property' },

    // === 枚举成员 ===
    { match: '\\benum\\s+([a-zA-Z_$][\\w$]*)', capture: 1, token: 'enumMember' },
    { match: '([a-zA-Z_$][\\w$]*)\\s*=', capture: 1, token: 'enumMember' },

    // === 命名空间 / 模块 ===
    { match: '\\b(namespace|module)\\s+([a-zA-Z_$][\\w$]*)', capture: 2, token: 'namespace' },

    // === 装饰器 ===
    { match: '(@[a-zA-Z_$][\\w$]*)', capture: 1, token: 'decorator' },
  ],
};

const BUILTIN_JAVASCRIPT_GRAMMAR: GrammarFile = {
  scope: 'javascript',
  rules: BUILTIN_TYPESCRIPT_GRAMMAR.rules.map((r) => ({ ...r })),
};

/** 内置插件定义 */
export const BUILTIN_PLUGINS: PluginPackage[] = [
  {
    name: 'ts-extra',
    display: 'TypeScript Extra Highlight',
    version: '1.0.0',
    description: 'Additional syntax highlighting for TypeScript: function names, class names, type annotations, etc.',
    author: 'AtomCode',
    languages: ['typescript', 'javascript'],
    builtin: true,
  },
];

// ============================================================================
// 状态
// ============================================================================

let installedPlugins: PluginPackage[] = [];
let initialized = false;

// ============================================================================
// IPC 封装
// ============================================================================

/** Electron 插件 IPC 接口 */
export interface PluginIpc {
  listPlugins: () => Promise<PluginPackage[]>;
  installPlugin: () => Promise<{ success: boolean; plugin?: PluginPackage; error?: string }>;
  uninstallPlugin: (name: string) => Promise<{ success: boolean; error?: string }>;
  readGrammar: (pluginPath: string) => Promise<GrammarFile | null>;
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
 */
export async function initializePlugins(): Promise<void> {
  if (initialized) return;
  initialized = true;

  // 注册内置 grammar
  addGrammarRules(BUILTIN_TYPESCRIPT_GRAMMAR.scope, BUILTIN_TYPESCRIPT_GRAMMAR.rules);
  addGrammarRules(BUILTIN_JAVASCRIPT_GRAMMAR.scope, BUILTIN_JAVASCRIPT_GRAMMAR.rules);

  // 加载已安装插件
  const ipc = getIpc();
  if (ipc) {
    try {
      const plugins = await ipc.listPlugins();
      installedPlugins = plugins;

      for (const plugin of plugins) {
        const grammar = await ipc.readGrammar(plugin.name);
        if (grammar) {
          addGrammarRules(grammar.scope, grammar.rules);
        }
      }
    } catch (err) {
      console.error('[pluginManager] Failed to load installed plugins:', err);
    }
  }

  // 刷新语法提供者
  await refreshGrammarProviders();

  console.debug(`[pluginManager] Initialized with ${installedPlugins.length} installed + ${BUILTIN_PLUGINS.length} builtin plugins`);
}

/**
 * 获取所有插件（内置 + 已安装）
 */
export function getAllPlugins(): PluginPackage[] {
  return [...BUILTIN_PLUGINS, ...installedPlugins];
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

    // 加载 grammar
    const grammar = await ipc.readGrammar(result.plugin.name);
    if (grammar) {
      addGrammarRules(grammar.scope, grammar.rules);
      await refreshGrammarProviders();
    }
  }
  return result;
}

/**
 * 卸载插件
 */
export async function uninstallPlugin(name: string): Promise<{ success: boolean; error?: string }> {
  // 内置插件不可卸载
  if (BUILTIN_PLUGINS.some((p) => p.name === name)) {
    return { success: false, error: 'Cannot uninstall built-in plugin' };
  }

  const ipc = getIpc();
  if (!ipc) return { success: false, error: 'Not running in Electron' };

  const result = await ipc.uninstallPlugin(name);
  if (result.success) {
    installedPlugins = installedPlugins.filter((p) => p.name !== name);
  }
  return result;
}
