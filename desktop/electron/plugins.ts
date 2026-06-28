/**
 * 插件文件系统操作（Electron 主进程）
 *
 * 负责读写 ~/.atomcode/plugins/ 目录：
 *   - 列出已安装插件
 *   - 安装（解压 zip）
 *   - 卸载（删除目录）
 *   - 读取 grammar 文件
 */

import * as path from 'path';
import * as fs from 'fs';

const PLUGINS_DIR = path.join(
  process.env.USERPROFILE || process.env.HOME || '~',
  '.atomcode',
  'plugins',
);

// ============================================================================
// 初始化
// ============================================================================

function ensurePluginsDir(): void {
  if (!fs.existsSync(PLUGINS_DIR)) {
    fs.mkdirSync(PLUGINS_DIR, { recursive: true });
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
}

// ============================================================================
// 操作函数
// ============================================================================

/**
 * 列出所有已安装的插件
 */
export function listPlugins(): PluginPackage[] {
  ensurePluginsDir();

  const results: PluginPackage[] = [];
  try {
    const entries = fs.readdirSync(PLUGINS_DIR, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;

      const pkgPath = path.join(PLUGINS_DIR, entry.name, 'package.json');
      if (!fs.existsSync(pkgPath)) continue;

      try {
        const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf-8')) as PluginPackage;
        pkg.name = entry.name;
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
 * 安装插件（从 .zip 文件路径安装）
 */
export function installPluginFromZip(zipPath: string): PluginPackage | null {
  ensurePluginsDir();

  try {
    // 使用 Node.js 内置的 zlib + 手动解析（或外部解压命令）
    // 这里用外部 unzip 命令兼容性更好，但 Electron 打包可能不包含
    // 方案：使用 Node.js 自带的 child_process + powershell 或 tar

    // 临时目录
    const tmpDir = path.join(PLUGINS_DIR, '.install-tmp');
    if (fs.existsSync(tmpDir)) {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
    fs.mkdirSync(tmpDir, { recursive: true });

    // 解压
    const { execSync } = require('child_process');
    const isWin = process.platform === 'win32';

    if (isWin) {
      // Windows: 使用 PowerShell 的 Expand-Archive
      execSync(
        `powershell -NoProfile -Command "Expand-Archive -Path '${zipPath}' -DestinationPath '${tmpDir}' -Force"`,
        { timeout: 30000 },
      );
    } else {
      // Unix: 使用 unzip
      execSync(`unzip -o "${zipPath}" -d "${tmpDir}"`, { timeout: 30000 });
    }

    // 读取 package.json — 可能在根目录或子目录
    let pkgDir = tmpDir;
    let pkgJsonPath = path.join(pkgDir, 'package.json');
    if (!fs.existsSync(pkgJsonPath)) {
      // 检查子目录
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

    // 如果已存在则删除旧版
    if (fs.existsSync(pluginDir)) {
      fs.rmSync(pluginDir, { recursive: true, force: true });
    }

    // 移动到插件目录
    fs.cpSync(pkgDir, pluginDir, { recursive: true });

    // 清理临时目录
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
    return true;
  } catch {
    return false;
  }
}
