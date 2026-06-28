/**
 * 插件文件系统操作（Electron 主进程）
 *
 * 负责读写 ~/.atomcode/plugins/ 目录：
 *   - 列出、安装、卸载插件
 *   - 启用/禁用状态持久化（含内置插件）
 *   - 读取 grammar 文件
 */

import * as path from 'path';
import * as fs from 'fs';

const PLUGINS_DIR = path.join(
  process.env.USERPROFILE || process.env.HOME || '~',
  '.atomcode',
  'plugins',
);

const CONFIG_PATH = path.join(PLUGINS_DIR, 'config.json');

interface PluginsConfig {
  /** 被禁用的插件名（含内置插件） */
  disabled: string[];
}

// ============================================================================
// 初始化
// ============================================================================

function ensurePluginsDir(): void {
  if (!fs.existsSync(PLUGINS_DIR)) {
    fs.mkdirSync(PLUGINS_DIR, { recursive: true });
  }
}

function readConfig(): PluginsConfig {
  try {
    if (fs.existsSync(CONFIG_PATH)) {
      return JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf-8'));
    }
  } catch {
    // fall through
  }
  return { disabled: [] };
}

function writeConfig(cfg: PluginsConfig): void {
  ensurePluginsDir();
  try {
    fs.writeFileSync(CONFIG_PATH, JSON.stringify(cfg, null, 2), 'utf-8');
  } catch (err) {
    console.error('[plugins] Failed to write config:', err);
  }
}

// ============================================================================
// 插件元数据
// ============================================================================

export interface PluginPackage {
  name: string;
  display: string;
  version: string;
  author?: string;
  description?: string;
  languages: string[];
  builtin?: boolean;
  installedAt?: number;
  /** 是否被禁用（持久化在 config.json） */
  disabled?: boolean;
}

// ============================================================================
// 操作函数
// ============================================================================

/**
 * 列出所有已安装的插件
 */
export function listPlugins(): PluginPackage[] {
  ensurePluginsDir();
  const cfg = readConfig();

  const results: PluginPackage[] = [];
  try {
    const entries = fs.readdirSync(PLUGINS_DIR, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      if (entry.name === '.install-tmp') continue;

      const pkgPath = path.join(PLUGINS_DIR, entry.name, 'package.json');
      if (!fs.existsSync(pkgPath)) continue;

      try {
        const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf-8')) as PluginPackage;
        pkg.name = entry.name;
        pkg.disabled = cfg.disabled.includes(pkg.name);
        results.push(pkg);
      } catch {
        // Skip malformed packages
      }
    }
  } catch {
    // Plugins dir not accessible
  }

  return results;
}

/**
 * 读取插件的 grammar 文件
 */
export function readGrammar(pluginName: string): Record<string, any> | null {
  const grammarPath = path.join(PLUGINS_DIR, pluginName, 'grammar.json');
  if (!fs.existsSync(grammarPath)) return null;

  try {
    return JSON.parse(fs.readFileSync(grammarPath, 'utf-8'));
  } catch {
    return null;
  }
}

/**
 * 检查某插件是否被禁用（供渲染进程查询内置插件状态）
 */
export function isPluginDisabled(name: string): boolean {
  return readConfig().disabled.includes(name);
}

/**
 * 设置插件的启用/禁用状态（持久化）
 */
export function setPluginDisabled(name: string, disabled: boolean): boolean {
  const cfg = readConfig();
  const idx = cfg.disabled.indexOf(name);
  if (disabled) {
    if (idx === -1) cfg.disabled.push(name);
  } else {
    if (idx !== -1) cfg.disabled.splice(idx, 1);
  }
  writeConfig(cfg);
  return true;
}

/**
 * 安装插件（从 .zip 文件路径安装）
 */
export function installPluginFromZip(zipPath: string): PluginPackage | null {
  ensurePluginsDir();

  try {
    const tmpDir = path.join(PLUGINS_DIR, '.install-tmp');
    if (fs.existsSync(tmpDir)) {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
    fs.mkdirSync(tmpDir, { recursive: true });

    const { execSync } = require('child_process');
    const isWin = process.platform === 'win32';

    if (isWin) {
      execSync(
        `powershell -NoProfile -Command "Expand-Archive -Path '${zipPath}' -DestinationPath '${tmpDir}' -Force"`,
        { timeout: 30000 },
      );
    } else {
      execSync(`unzip -o "${zipPath}" -d "${tmpDir}"`, { timeout: 30000 });
    }

    let pkgDir = tmpDir;
    let pkgJsonPath = path.join(pkgDir, 'package.json');
    if (!fs.existsSync(pkgJsonPath)) {
      const subDirs = fs.readdirSync(tmpDir, { withFileTypes: true })
        .filter((d: fs.Dirent) => d.isDirectory());
      for (const sub of subDirs) {
        const candidate = path.join(tmpDir, sub.name, 'package.json');
        if (fs.existsSync(candidate)) {
          pkgDir = path.join(tmpDir, sub.name);
          pkgJsonPath = candidate;
          break;
        }
      }
    }

    if (!fs.existsSync(pkgJsonPath)) {
      fs.rmSync(tmpDir, { recursive: true, force: true });
      return null;
    }

    const pkg = JSON.parse(fs.readFileSync(pkgJsonPath, 'utf-8')) as PluginPackage;
    const pluginDir = path.join(PLUGINS_DIR, pkg.name || path.basename(zipPath, '.zip'));

    if (fs.existsSync(pluginDir)) {
      fs.rmSync(pluginDir, { recursive: true, force: true });
    }

    fs.cpSync(pkgDir, pluginDir, { recursive: true });
    fs.rmSync(tmpDir, { recursive: true, force: true });

    pkg.installedAt = Date.now();
    return pkg;
  } catch (err) {
    console.error('[plugins] Install failed:', err);
    return null;
  }
}

/**
 * 卸载插件
 */
export function uninstallPlugin(name: string): boolean {
  const pluginDir = path.join(PLUGINS_DIR, name);
  if (!fs.existsSync(pluginDir)) return false;

  try {
    fs.rmSync(pluginDir, { recursive: true, force: true });
    setPluginDisabled(name, false);
    return true;
  } catch {
    return false;
  }
}
