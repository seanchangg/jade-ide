import { contextBridge, ipcRenderer } from 'electron';
import { IPC } from '../shared/types';

// Type-safe bridge between renderer and main process.
// Every call is async IPC — the renderer never blocks.

const forgeAPI = {
  workspace: {
    openFolderDialog: (): Promise<string | null> =>
      ipcRenderer.invoke(IPC.OPEN_FOLDER_DIALOG),

    open: (folderPath: string): Promise<void> =>
      ipcRenderer.invoke(IPC.OPEN_WORKSPACE, folderPath),

    getTree: (): Promise<any> =>
      ipcRenderer.invoke(IPC.GET_FILE_TREE),

    readFile: (filePath: string): Promise<string> =>
      ipcRenderer.invoke(IPC.READ_FILE, filePath),

    writeFile: (filePath: string, content: string): Promise<void> =>
      ipcRenderer.invoke(IPC.WRITE_FILE, filePath, content),

    rename: (oldPath: string, newPath: string): Promise<void> =>
      ipcRenderer.invoke(IPC.RENAME_FILE, oldPath, newPath),

    delete: (filePath: string): Promise<void> =>
      ipcRenderer.invoke(IPC.DELETE_FILE, filePath),

    createFile: (filePath: string): Promise<void> =>
      ipcRenderer.invoke(IPC.CREATE_FILE, filePath),

    createDir: (dirPath: string): Promise<void> =>
      ipcRenderer.invoke(IPC.CREATE_DIR, dirPath),

    onFileChanged: (callback: (event: any) => void): void => {
      ipcRenderer.on(IPC.FILE_CHANGED, (_e, event) => callback(event));
    },
  },

  lsp: {
    initialize: (rootPath: string): Promise<void> =>
      ipcRenderer.invoke(IPC.LSP_INITIALIZE, rootPath),

    completion: (uri: string, line: number, character: number): Promise<any[]> =>
      ipcRenderer.invoke(IPC.LSP_COMPLETION, uri, line, character),

    hover: (uri: string, line: number, character: number): Promise<any> =>
      ipcRenderer.invoke(IPC.LSP_HOVER, uri, line, character),

    definition: (uri: string, line: number, character: number): Promise<any[]> =>
      ipcRenderer.invoke(IPC.LSP_DEFINITION, uri, line, character),

    references: (uri: string, line: number, character: number): Promise<any[]> =>
      ipcRenderer.invoke(IPC.LSP_REFERENCES, uri, line, character),

    didOpen: (uri: string, content: string, version: number): void => {
      ipcRenderer.send(IPC.LSP_DID_OPEN, uri, content, version);
    },

    didChange: (uri: string, changes: any[], version: number): void => {
      ipcRenderer.send(IPC.LSP_DID_CHANGE, uri, changes, version);
    },

    didClose: (uri: string): void => {
      ipcRenderer.send(IPC.LSP_DID_CLOSE, uri);
    },

    memoryInfo: (uri: string, line: number, character: number): Promise<any> =>
      ipcRenderer.invoke(IPC.LSP_MEMORY_INFO, uri, line, character),

    onDiagnostics: (callback: (data: any) => void): void => {
      ipcRenderer.on(IPC.LSP_DIAGNOSTICS, (_e, data) => callback(data));
    },
  },

  terminal: {
    create: (cwd: string): Promise<number> =>
      ipcRenderer.invoke(IPC.TERMINAL_CREATE, cwd),

    write: (id: number, data: string): void => {
      ipcRenderer.send(IPC.TERMINAL_WRITE, id, data);
    },

    resize: (id: number, cols: number, rows: number): void => {
      ipcRenderer.send(IPC.TERMINAL_RESIZE, id, cols, rows);
    },

    onData: (callback: (id: number, data: string) => void): void => {
      ipcRenderer.on(IPC.TERMINAL_DATA, (_e, id, data) => callback(id, data));
    },

    onExit: (callback: (id: number, code: number) => void): void => {
      ipcRenderer.on(IPC.TERMINAL_EXIT, (_e, id, code) => callback(id, code));
    },

    destroy: (id: number): void => {
      ipcRenderer.send(IPC.TERMINAL_DESTROY, id);
    },
  },

  system: {
    start: (): void => { ipcRenderer.send(IPC.SYSTEM_START); },
    stop: (): void => { ipcRenderer.send(IPC.SYSTEM_STOP); },
    onStats: (callback: (stats: any) => void): void => {
      ipcRenderer.on(IPC.SYSTEM_STATS, (_e, stats) => callback(stats));
    },
  },

  build: {
    compile: (filePath: string, flags?: string[], sanitize?: boolean, instrument?: boolean): Promise<any> =>
      ipcRenderer.invoke(IPC.BUILD_COMPILE, filePath, flags, sanitize, instrument),

    run: (config: any): Promise<any> =>
      ipcRenderer.invoke(IPC.BUILD_RUN, config),

    stop: (): void => {
      ipcRenderer.send(IPC.BUILD_STOP);
    },

    getAsm: (filePath: string, extraFlags?: string[]): Promise<any> =>
      ipcRenderer.invoke(IPC.BUILD_ASM, filePath, extraFlags),

    onOutput: (callback: (data: string) => void): void => {
      ipcRenderer.on(IPC.BUILD_OUTPUT, (_e, data) => callback(data));
    },

    onMemoryEvent: (callback: (event: any) => void): void => {
      ipcRenderer.on(IPC.BUILD_MEMORY_EVENT, (_e, event) => callback(event));
    },

    onTrainingScalar: (callback: (data: any) => void): void => {
      ipcRenderer.on(IPC.BUILD_TRAINING_SCALAR, (_e, data) => callback(data));
    },

    onTrainingTiming: (callback: (data: any) => void): void => {
      ipcRenderer.on(IPC.BUILD_TRAINING_TIMING, (_e, data) => callback(data));
    },
  },

  telemetry: {
    // main → renderer: a scalar/timer/buffer name was auto-discovered
    onDecl: (callback: (decl: any) => void): void => {
      ipcRenderer.on(IPC.TELEMETRY_DECL, (_e, decl) => callback(decl));
    },

    // main → renderer: a tracked GPU-buffer frame (base64 float32)
    onTensor: (callback: (tensor: any) => void): void => {
      ipcRenderer.on(IPC.TELEMETRY_TENSOR, (_e, tensor) => callback(tensor));
    },

    // renderer → main: toggle tracking for an item (checkbox / startup restore)
    setTrack: (track: any): void => {
      ipcRenderer.send(IPC.TELEMETRY_TRACK, track);
    },
  },

  debug: {
    start: (executable: string, cwd: string, breakpoints: any[]): Promise<void> =>
      ipcRenderer.invoke(IPC.DEBUG_START, executable, cwd, breakpoints),
    stop: (): Promise<void> => ipcRenderer.invoke(IPC.DEBUG_STOP),
    continue: (): Promise<void> => ipcRenderer.invoke(IPC.DEBUG_CONTINUE),
    stepOver: (): Promise<void> => ipcRenderer.invoke(IPC.DEBUG_STEP_OVER),
    stepInto: (): Promise<void> => ipcRenderer.invoke(IPC.DEBUG_STEP_INTO),
    stepOut: (): Promise<void> => ipcRenderer.invoke(IPC.DEBUG_STEP_OUT),
    setBreakpoint: (file: string, line: number): Promise<void> =>
      ipcRenderer.invoke(IPC.DEBUG_SET_BREAKPOINT, file, line),
    removeBreakpoint: (file: string, line: number): Promise<void> =>
      ipcRenderer.invoke(IPC.DEBUG_REMOVE_BREAKPOINT, file, line),
    getVarChildren: (expr: string): Promise<any[]> =>
      ipcRenderer.invoke(IPC.DEBUG_VAR_CHILDREN, expr),
    onStopped: (callback: (event: any) => void): void => {
      ipcRenderer.on(IPC.DEBUG_STOPPED, (_e, event) => callback(event));
    },
    onOutput: (callback: (data: string) => void): void => {
      ipcRenderer.on(IPC.DEBUG_OUTPUT, (_e, data) => callback(data));
    },
    onExited: (callback: (code: number) => void): void => {
      ipcRenderer.on(IPC.DEBUG_EXITED, (_e, code) => callback(code));
    },
  },

  ai: {
    infill: (req: { prefix: string; suffix: string; filename?: string }): Promise<{ content: string } | null> =>
      ipcRenderer.invoke(IPC.AI_INFILL, req),

    status: (): Promise<any> =>
      ipcRenderer.invoke(IPC.AI_STATUS),

    setEnabled: (enabled: boolean): Promise<void> =>
      ipcRenderer.invoke(IPC.AI_SET_ENABLED, enabled),

    setModel: (id: string): Promise<void> =>
      ipcRenderer.invoke(IPC.AI_SET_MODEL, id),

    onStatusChanged: (callback: (status: any) => void): void => {
      ipcRenderer.on(IPC.AI_STATUS_CHANGED, (_e, status) => callback(status));
    },
  },

  state: {
    save: (key: string, value: any): Promise<void> =>
      ipcRenderer.invoke(IPC.STATE_SAVE, key, value),

    load: (key: string): Promise<any> =>
      ipcRenderer.invoke(IPC.STATE_LOAD, key),
  },
};

contextBridge.exposeInMainWorld('forge', forgeAPI);

// Type export for renderer usage
export type ForgeAPI = typeof forgeAPI;
