import { contextBridge, ipcRenderer, IpcRendererEvent } from 'electron';

contextBridge.exposeInMainWorld('electronAPI', {
  // Daemon control
  getDaemonPort: (): Promise<number | null> => ipcRenderer.invoke('daemon:getPort'),
  isDaemonRunning: (): Promise<boolean> => ipcRenderer.invoke('daemon:isRunning'),

  // Dialogs
  openFolderDialog: (): Promise<string | null> => ipcRenderer.invoke('dialog:openFolder'),

  // App info
  getVersion: (): Promise<string> => ipcRenderer.invoke('app:getVersion'),

  // Events from main process
  onOpenFolder: (callback: (path: string) => void) => {
    const handler = (_event: IpcRendererEvent, path: string) => callback(path);
    ipcRenderer.on('open-folder', handler);
    return () => ipcRenderer.removeListener('open-folder', handler);
  },

  // ─── Plugin System ──────────────────────────────────────────────────────
  plugins: {
    listPlugins: (): Promise<any[]> => ipcRenderer.invoke('plugins:list'),
    installPlugin: (): Promise<any> => ipcRenderer.invoke('plugins:install'),
    uninstallPlugin: (name: string): Promise<any> => ipcRenderer.invoke('plugins:uninstall', name),
    readGrammar: (name: string): Promise<any> => ipcRenderer.invoke('plugins:readGrammar', name),
    isDisabled: (name: string): Promise<boolean> => ipcRenderer.invoke('plugins:isDisabled', name),
    setDisabled: (name: string, disabled: boolean): Promise<{ success: boolean }> =>
      ipcRenderer.invoke('plugins:setDisabled', name, disabled),
  },
});
