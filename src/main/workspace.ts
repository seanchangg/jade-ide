import { ipcMain, BrowserWindow } from 'electron';
import * as fs from 'fs';
import * as path from 'path';
import { FileNode, FileChangeEvent, IPC, WorkspaceState } from '../shared/types';

const IGNORED_DIRS = new Set([
  'node_modules', '.git', '.svn', '.hg', '.DS_Store',
  'build', 'cmake-build-debug', 'cmake-build-release', 'cmake-build-forge',
  '.cache', '__pycache__', '.vscode', '.idea',
]);

const IGNORED_EXTENSIONS = new Set([
  '.o', '.obj', '.a', '.lib', '.so', '.dylib',
  '.exe', '.out', '.class', '.pyc',
]);

export class Workspace {
  private rootPath: string | null = null;
  private watcher: fs.FSWatcher | null = null;
  private stateFilePath: string | null = null;
  private stateCache: Record<string, any> = {};
  private stateDirty = false;
  private stateFlushTimer: ReturnType<typeof setTimeout> | null = null;

  get root(): string | null {
    return this.rootPath;
  }

  async open(folderPath: string): Promise<void> {
    this.close();
    this.rootPath = folderPath;
    this.stateFilePath = path.join(folderPath, '.forge', 'workspace.json');
    this.loadStateFromDisk();
  }

  close(): void {
    if (this.watcher) {
      this.watcher.close();
      this.watcher = null;
    }
    this.flushState();
    this.rootPath = null;
    this.stateFilePath = null;
    this.stateCache = {};
  }

  startWatching(onChange: (event: FileChangeEvent) => void): void {
    if (!this.rootPath) return;

    this.watcher = fs.watch(
      this.rootPath,
      { recursive: true },
      (eventType, filename) => {
        if (!filename) return;
        const fullPath = path.join(this.rootPath!, filename);

        // Skip ignored paths
        const parts = filename.split(path.sep);
        if (parts.some((p) => IGNORED_DIRS.has(p))) return;
        if (IGNORED_EXTENSIONS.has(path.extname(filename))) return;

        const changeType = eventType === 'rename'
          ? (fs.existsSync(fullPath) ? 'create' : 'delete')
          : 'change';

        onChange({ type: changeType, path: fullPath });
      }
    );
  }

  async getTree(dirPath?: string, depth = 0): Promise<FileNode[]> {
    const target = dirPath || this.rootPath;
    if (!target) return [];

    // Don't recurse too deep on initial load
    const MAX_DEPTH = depth === 0 ? 3 : 1;

    try {
      const entries = await fs.promises.readdir(target, { withFileTypes: true });
      const nodes: FileNode[] = [];

      // Sort: directories first, then alphabetical
      const sorted = entries.sort((a, b) => {
        if (a.isDirectory() && !b.isDirectory()) return -1;
        if (!a.isDirectory() && b.isDirectory()) return 1;
        return a.name.localeCompare(b.name);
      });

      for (const entry of sorted) {
        if (IGNORED_DIRS.has(entry.name)) continue;
        if (entry.name.startsWith('.') && entry.name !== '.forge') continue;
        if (entry.name.endsWith('.dSYM')) continue;
        // Skip extensionless files at root depth (likely compiled executables)
        if (depth === 0 && entry.isFile() && !entry.name.includes('.')) continue;

        const fullPath = path.join(target, entry.name);
        const isDir = entry.isDirectory();

        const node: FileNode = {
          name: entry.name,
          path: fullPath,
          isDirectory: isDir,
        };

        if (isDir && depth < MAX_DEPTH) {
          node.children = await this.getTree(fullPath, depth + 1);
        } else if (isDir) {
          node.children = []; // placeholder, lazy load later
        }

        if (!isDir && IGNORED_EXTENSIONS.has(path.extname(entry.name))) continue;

        nodes.push(node);
      }

      return nodes;
    } catch {
      return [];
    }
  }

  async readFile(filePath: string): Promise<string> {
    return fs.promises.readFile(filePath, 'utf-8');
  }

  async writeFile(filePath: string, content: string): Promise<void> {
    await fs.promises.writeFile(filePath, content, 'utf-8');
  }

  async renameFile(oldPath: string, newPath: string): Promise<void> {
    await fs.promises.rename(oldPath, newPath);
  }

  async deleteFile(filePath: string): Promise<void> {
    const stat = await fs.promises.stat(filePath);
    if (stat.isDirectory()) {
      await fs.promises.rm(filePath, { recursive: true });
    } else {
      await fs.promises.unlink(filePath);
    }
  }

  async createFile(filePath: string): Promise<void> {
    await fs.promises.writeFile(filePath, '', 'utf-8');
  }

  async createDir(dirPath: string): Promise<void> {
    await fs.promises.mkdir(dirPath, { recursive: true });
  }

  // ── State persistence ──

  saveState(key: string, value: any): void {
    this.stateCache[key] = value;
    this.stateDirty = true;
    this.scheduleFlush();
  }

  loadState(key: string): any {
    return this.stateCache[key] ?? null;
  }

  private scheduleFlush(): void {
    if (this.stateFlushTimer) return;
    this.stateFlushTimer = setTimeout(() => {
      this.flushState();
      this.stateFlushTimer = null;
    }, 2000); // debounce 2s
  }

  private flushState(): void {
    if (!this.stateDirty || !this.stateFilePath) return;
    try {
      const dir = path.dirname(this.stateFilePath);
      if (!fs.existsSync(dir)) {
        fs.mkdirSync(dir, { recursive: true });
      }
      fs.writeFileSync(this.stateFilePath, JSON.stringify(this.stateCache, null, 2));
      this.stateDirty = false;
    } catch (err) {
      console.error('Failed to flush workspace state:', err);
    }
  }

  private loadStateFromDisk(): void {
    if (!this.stateFilePath) return;
    try {
      if (fs.existsSync(this.stateFilePath)) {
        const raw = fs.readFileSync(this.stateFilePath, 'utf-8');
        this.stateCache = JSON.parse(raw);
      }
    } catch {
      this.stateCache = {};
    }
  }
}

export function registerWorkspaceHandlers(workspace: Workspace, win: BrowserWindow): void {
  ipcMain.handle(IPC.OPEN_WORKSPACE, async (_e, folderPath: string) => {
    await workspace.open(folderPath);
    workspace.startWatching((event) => {
      if (!win.isDestroyed()) {
        win.webContents.send(IPC.FILE_CHANGED, event);
      }
    });
  });

  ipcMain.handle(IPC.GET_FILE_TREE, async (_e, dirPath?: string) => {
    return workspace.getTree(dirPath);
  });

  ipcMain.handle(IPC.READ_FILE, async (_e, filePath: string) => {
    return workspace.readFile(filePath);
  });

  ipcMain.handle(IPC.WRITE_FILE, async (_e, filePath: string, content: string) => {
    return workspace.writeFile(filePath, content);
  });

  ipcMain.handle(IPC.RENAME_FILE, async (_e, oldPath: string, newPath: string) => {
    return workspace.renameFile(oldPath, newPath);
  });

  ipcMain.handle(IPC.DELETE_FILE, async (_e, filePath: string) => {
    return workspace.deleteFile(filePath);
  });

  ipcMain.handle(IPC.CREATE_FILE, async (_e, filePath: string) => {
    return workspace.createFile(filePath);
  });

  ipcMain.handle(IPC.CREATE_DIR, async (_e, dirPath: string) => {
    return workspace.createDir(dirPath);
  });
}
