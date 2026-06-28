import { app, BrowserWindow, ipcMain, dialog, Menu, MenuItemConstructorOptions } from 'electron';
import * as path from 'path';
import { DaemonManager } from './daemon';
import { listPlugins, installPluginFromZip, uninstallPlugin, readGrammar, isPluginDisabled, setPluginDisabled } from './plugins';

let mainWindow: BrowserWindow | null = null;
let daemon: DaemonManager | null = null;

const isDev = process.env.NODE_ENV === 'development' || process.argv.includes('--dev');

function createMenu() {
  const isMac = process.platform === 'darwin';
  const template: MenuItemConstructorOptions[] = [
    ...(isMac ? [{
      label: app.name,
      submenu: [
        { role: 'about' as const },
        { type: 'separator' as const },
        { role: 'quit' as const },
      ],
    }] : []),
    {
      label: 'File',
      submenu: [
        {
          label: 'Open Folder...', 
          accelerator: 'CmdOrCtrl+O',
          click: async () => {
            const result = await dialog.showOpenDialog(mainWindow!, {
              properties: ['openDirectory'],
            });
            if (!result.canceled && result.filePaths.length > 0) {
              mainWindow?.webContents.send('open-folder', result.filePaths[0]);
            }
          },
        },
        { type: 'separator' },
        isMac ? { role: 'close' } : { role: 'quit' },
      ],
    },
    {
      label: 'Edit',
      submenu: [
        { role: 'undo' },
        { role: 'redo' },
        { type: 'separator' },
        { role: 'cut' },
        { role: 'copy' },
        { role: 'paste' },
        { role: 'selectAll' },
      ],
    },
    {
      label: 'View',
      submenu: [
        { role: 'reload' },
        { role: 'forceReload' },
        { role: 'toggleDevTools' },
        { type: 'separator' },
        { role: 'resetZoom' },
        { role: 'zoomIn' },
        { role: 'zoomOut' },
        { type: 'separator' },
        { role: 'togglefullscreen' },
      ],
    },
    {
      label: 'Help',
      submenu: [
        {
          label: 'About AtomCode IDE',
          click: () => {
            dialog.showMessageBox(mainWindow!, {
              type: 'info',
              title: 'About AtomCode IDE',
              message: `AtomCode IDE v${app.getVersion()}`,
              detail: 'AI-powered code editor built on AtomCode agent.\n\n100% AI-generated.',
            });
          },
        },
      ],
    },
  ];
  const menu = Menu.buildFromTemplate(template);
  Menu.setApplicationMenu(menu);
}

async function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1400,
    height: 900,
    minWidth: 800,
    minHeight: 600,
    title: 'AtomCode IDE',
    icon: path.join(__dirname, '../public/icon.png'),
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      webSecurity: true,
    },
    show: false,
    backgroundColor: '#1e1e1e',
  });

  mainWindow.once('ready-to-show', () => {
    mainWindow?.show();
  });

  // Force-set CSP headers to allow Monaco Editor from CDN
  mainWindow.webContents.session.webRequest.onHeadersReceived((details, callback) => {
    const csp =
      "default-src 'self'; " +
      "script-src 'self' 'unsafe-inline' 'unsafe-eval' https://cdn.jsdelivr.net; " +
      "script-src-elem 'self' 'unsafe-inline' 'unsafe-eval' https://cdn.jsdelivr.net; " +
      "style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; " +
      "font-src 'self' data: blob:; " +
      "img-src 'self' data: blob:; " +
      "connect-src 'self' http://127.0.0.1:* ws://127.0.0.1:* https://cdn.jsdelivr.net; " +
      "worker-src 'self' blob: https://cdn.jsdelivr.net;";

    callback({
      responseHeaders: {
        ...details.responseHeaders,
        'Content-Security-Policy': [csp],
      },
    });
  });

  // Load the renderer
  if (isDev) {
    mainWindow.loadURL('http://localhost:5173');
    mainWindow.webContents.openDevTools();
  } else {
    mainWindow.loadFile(path.join(__dirname, '../dist/index.html'));
  }

  mainWindow.on('closed', () => {
    mainWindow = null;
  });
}

// ─── IPC Handlers ───────────────────────────────────────────────────────────

ipcMain.handle('daemon:getPort', () => {
  return daemon?.getPort() ?? null;
});

ipcMain.handle('daemon:isRunning', () => {
  return daemon?.isRunning() ?? false;
});

ipcMain.handle('dialog:openFolder', async () => {
  const result = await dialog.showOpenDialog(mainWindow!, {
    properties: ['openDirectory'],
  });
  if (!result.canceled && result.filePaths.length > 0) {
    return result.filePaths[0];
  }
  return null;
});

// ─── Plugin IPC Handlers ────────────────────────────────────────────────

ipcMain.handle('plugins:list', () => {
  return listPlugins();
});

ipcMain.handle('plugins:install', async () => {
  const result = await dialog.showOpenDialog(mainWindow!, {
    properties: ['openFile'],
    filters: [{ name: 'Plugin Package', extensions: ['zip'] }],
  });
  if (result.canceled || result.filePaths.length === 0) {
    return { success: false, error: 'Cancelled' };
  }
  const plugin = installPluginFromZip(result.filePaths[0]);
  if (plugin) {
    return { success: true, plugin };
  }
  return { success: false, error: 'Failed to install plugin. Ensure the zip contains a valid plugin package (package.json + grammar.json).' };
});

ipcMain.handle('plugins:uninstall', (_event, name: string) => {
  const ok = uninstallPlugin(name);
  return { success: ok, error: ok ? undefined : 'Plugin not found or could not be removed' };
});

ipcMain.handle('plugins:readGrammar', (_event, name: string) => {
  return readGrammar(name);
});

ipcMain.handle('plugins:isDisabled', (_event, name: string) => {
  return isPluginDisabled(name);
});

ipcMain.handle('plugins:setDisabled', (_event, name: string, disabled: boolean) => {
  setPluginDisabled(name, disabled);
  return { success: true };
});

ipcMain.handle('app:getVersion', () => {
  return app.getVersion();
});

// ─── App Lifecycle ──────────────────────────────────────────────────────────

app.whenReady().then(async () => {
  createMenu();

  // Start the atomcode-daemon
  daemon = new DaemonManager();
  try {
    await daemon.start();
    console.log(`[daemon] started on port ${daemon.getPort()}`);
  } catch (err) {
    console.error('[daemon] failed to start:', err);
    // Continue anyway — the renderer will show an error state
  }

  await createWindow();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) {
      createWindow();
    }
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') {
    app.quit();
  }
});

app.on('before-quit', async () => {
  if (daemon) {
    await daemon.stop();
    daemon = null;
  }
});

// Handle open-folder from menu (forward to renderer)
ipcMain.on('open-folder', (_event: Electron.IpcMainEvent, path: string) => {
  mainWindow?.webContents.send('open-folder', path);
});
