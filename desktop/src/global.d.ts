/// <reference types="vite/client" />

interface PluginIpc {
  listPlugins: () => Promise<any[]>;
  installPlugin: () => Promise<{ success: boolean; plugin?: any; error?: string }>;
  uninstallPlugin: (name: string) => Promise<{ success: boolean; error?: string }>;
  readGrammar: (name: string) => Promise<any>;
}

interface ElectronAPI {
  getDaemonPort: () => Promise<number | null>;
  isDaemonRunning: () => Promise<boolean>;
  openFolderDialog: () => Promise<string | null>;
  getVersion: () => Promise<string>;
  onOpenFolder: (callback: (path: string) => void) => () => void;
  plugins?: PluginIpc;
}

export {}; // ensure this file is treated as a module

declare global {
  interface Window {
    electronAPI?: ElectronAPI;
  }
}
