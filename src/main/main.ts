import { app, BrowserWindow, ipcMain, dialog, nativeTheme } from 'electron';
import * as path from 'path';
import * as fs from 'fs';
import { registerWorkspaceHandlers, Workspace } from './workspace';
import { registerLspHandlers, LspClient } from './lsp-client';
import { registerPtyHandlers, PtyManager } from './pty-manager';
import { SystemMonitor } from './system-monitor';
import { registerBuildHandlers } from './build-runner';
import { registerDebugHandlers } from './debug-driver';
import { registerTelemetryHandlers, shutdownTelemetry } from './telemetry-server';
import { InlineCompletionBackend, registerAiHandlers } from './inline-completion';
import { IPC } from '../shared/types';

// Global config stored in app data, survives across workspaces
const GLOBAL_CONFIG_PATH = path.join(app.getPath('userData'), 'forge-config.json');

function loadGlobalConfig(): Record<string, any> {
  try {
    if (fs.existsSync(GLOBAL_CONFIG_PATH)) {
      return JSON.parse(fs.readFileSync(GLOBAL_CONFIG_PATH, 'utf-8'));
    }
  } catch {}
  return {};
}

function saveGlobalConfig(config: Record<string, any>): void {
  try {
    fs.writeFileSync(GLOBAL_CONFIG_PATH, JSON.stringify(config, null, 2));
  } catch (err) {
    console.error('Failed to save global config:', err);
  }
}

let mainWindow: BrowserWindow | null = null;
let workspace: Workspace | null = null;
let lspClient: LspClient | null = null;
let ptyManager: PtyManager | null = null;
let systemMonitor: SystemMonitor | null = null;
let aiBackend: InlineCompletionBackend | null = null;

function createWindow(): void {
  mainWindow = new BrowserWindow({
    width: 1400,
    height: 900,
    minWidth: 800,
    minHeight: 500,
    title: 'Jade',
    titleBarStyle: 'hiddenInset',
    trafficLightPosition: { x: 12, y: 12 },
    backgroundColor: '#1E1F22',
    show: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false, // needed for node-pty IPC
    },
  });

  mainWindow.loadFile(path.join(__dirname, '../../renderer/index.html'));

  // Show when ready to avoid white flash
  mainWindow.once('ready-to-show', () => {
    mainWindow!.show();
    // mainWindow!.webContents.openDevTools({ mode: 'detach' });
  });

  mainWindow.on('closed', () => {
    mainWindow = null;
  });
}

function initializeServices(): void {
  if (!mainWindow) return;

  workspace = new Workspace();
  lspClient = new LspClient();
  ptyManager = new PtyManager();
  systemMonitor = new SystemMonitor();

  registerWorkspaceHandlers(workspace, mainWindow);
  registerLspHandlers(lspClient, mainWindow);
  registerPtyHandlers(ptyManager, mainWindow);
  registerBuildHandlers(mainWindow);
  registerDebugHandlers(mainWindow);

  // AI inline completion — starts searching for / spawning a FIM server in
  // the background; the renderer reacts to status events as it comes up.
  aiBackend = new InlineCompletionBackend();
  registerAiHandlers(aiBackend, mainWindow);
  aiBackend.start();

  // Telemetry socket server (must exist before any program is run so
  // build-runner can advertise FORGE_TELEMETRY_SOCK to spawned processes).
  registerTelemetryHandlers(mainWindow);

  // Dialog: open folder
  ipcMain.handle(IPC.OPEN_FOLDER_DIALOG, async () => {
    if (!mainWindow) return null;
    const result = await dialog.showOpenDialog(mainWindow, {
      properties: ['openDirectory'],
      title: 'Open Workspace',
    });
    if (result.canceled || result.filePaths.length === 0) return null;
    return result.filePaths[0];
  });

  // System monitoring
  ipcMain.on(IPC.SYSTEM_START, () => {
    systemMonitor!.start((stats) => {
      if (mainWindow && !mainWindow.isDestroyed()) {
        mainWindow.webContents.send(IPC.SYSTEM_STATS, stats);
      }
    });
  });

  ipcMain.on(IPC.SYSTEM_STOP, () => {
    systemMonitor!.stop();
  });

  // State persistence — workspaceRoot and xpTotal are global, everything else is per-workspace
  const GLOBAL_KEYS = new Set(['workspaceRoot', 'xpTotal', 'theme', 'aiModel', 'aiMultiline']);

  ipcMain.handle(IPC.STATE_SAVE, async (_e, key: string, value: any) => {
    if (GLOBAL_KEYS.has(key)) {
      const config = loadGlobalConfig();
      if (key === 'workspaceRoot') config.lastWorkspace = value;
      else config[key] = value;
      saveGlobalConfig(config);
    } else if (workspace) {
      workspace.saveState(key, value);
    }
  });

  ipcMain.handle(IPC.STATE_LOAD, async (_e, key: string) => {
    if (GLOBAL_KEYS.has(key)) {
      const config = loadGlobalConfig();
      if (key === 'workspaceRoot') return config.lastWorkspace || null;
      return config[key] ?? null;
    }
    if (workspace) {
      return workspace.loadState(key);
    }
    return null;
  });
}

// ── App lifecycle ──

app.setName('Jade');

app.whenReady().then(() => {
  nativeTheme.themeSource = 'dark';
  createWindow();
  initializeServices();
});

app.on('window-all-closed', () => {
  lspClient?.shutdown();
  ptyManager?.destroyAll();
  systemMonitor?.stop();
  aiBackend?.shutdown();
  shutdownTelemetry();
  app.quit();
});

app.on('will-quit', () => {
  aiBackend?.shutdown();
  shutdownTelemetry();
});

app.on('activate', () => {
  if (BrowserWindow.getAllWindows().length === 0) {
    createWindow();
    initializeServices();
  }
});
