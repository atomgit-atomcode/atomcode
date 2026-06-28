/**
 * Monaco Editor 配置
 *
 * 适配 Electron CSP 环境，确保 TypeScript/JavaScript 等语言的高亮 Worker 能正确加载。
 * Electron 的 onHeadersReceived CSP 会覆盖所有响应头，Monaco 默认的 Worker 创建机制
 * 可能会被 CSP 阻塞，需要显式配置 MonacoEnvironment.getWorker 来绕过。
 *
 * 注意：Monaco 0.52.x 将 Worker 文件名从 ts.worker.js 改为 tsWorker.js，
 * 且 TypeScript/JavaScript 的语法高亮完全依赖 Worker（不像 JSON/CSS 有内核 fallback），
 * 因此 Worker URL 必须正确，否则 TS/TSX 文件会完全无高亮。
 */
import { loader } from '@monaco-editor/react';

const MONACO_VERSION = '0.52.2';
const CDN_BASE = `https://cdn.jsdelivr.net/npm/monaco-editor@${MONACO_VERSION}/min/vs`;

// 配置加载路径，避免自动 URL 解析失败
loader.config({ paths: { vs: CDN_BASE } });

// 在 Monaco 初始化前设置环境，代理 Worker 创建
// Monaco 0.52.x 的 Worker 文件名已改为 tsWorker.js / jsonWorker.js 等（去掉了中间的 dot）
// 详见 https://github.com/microsoft/monaco-editor/blob/main/CHANGELOG.md
const monacoEnvironment: { getWorker: (workerId: string, label: string) => Worker } = {
  getWorker(_workerId: string, label: string) {
    let workerUrl: string;
    switch (label) {
      case 'typescript':
      case 'javascript':
        workerUrl = `${CDN_BASE}/language/typescript/tsWorker.js`;
        break;
      case 'json':
        workerUrl = `${CDN_BASE}/language/json/jsonWorker.js`;
        break;
      case 'css':
      case 'scss':
      case 'less':
        workerUrl = `${CDN_BASE}/language/css/cssWorker.js`;
        break;
      case 'html':
      case 'handlebars':
      case 'razor':
        workerUrl = `${CDN_BASE}/language/html/htmlWorker.js`;
        break;
      default:
        // Monaco 0.52.x 将基础 Worker 移至 base/worker/workerMain.js（不再有 editor/editorWorker.js）
        workerUrl = `${CDN_BASE}/base/worker/workerMain.js`;
        break;
    }
    console.debug(`[Monaco] Creating worker for "${label}" →`, workerUrl);
    return new Worker(workerUrl);
  },
};

// 挂载到全局 — Monaco 加载后会读取这里的配置
window.MonacoEnvironment = monacoEnvironment;
