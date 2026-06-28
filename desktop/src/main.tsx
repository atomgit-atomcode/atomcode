import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './App';
import './styles/app.css';
import './monacoSetup'; // Monaco Editor 配置 — 在渲染前初始化 Worker 和加载器
import { initializePlugins } from './plugins/pluginManager'; // 插件系统

// Wait for the daemon API to be reachable and then render
async function main() {
  // Expose the daemon port — Electron main process manages the daemon lifecycle
  const apiPort = await window.electronAPI?.getDaemonPort() ?? 13456;
  const baseUrl = `http://127.0.0.1:${apiPort}`;

  // 初始化插件系统（注册内置语法规则 + 加载已安装插件）
  initializePlugins().catch(console.error);

  const root = ReactDOM.createRoot(document.getElementById('root')!);
  root.render(
    <React.StrictMode>
      <App apiBaseUrl={baseUrl} />
    </React.StrictMode>
  );
}

main().catch(console.error);
