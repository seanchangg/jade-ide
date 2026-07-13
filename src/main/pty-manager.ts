import { ipcMain, BrowserWindow } from 'electron';
import { IPC } from '../shared/types';

// node-pty is a native module — import dynamically to handle missing builds gracefully
let pty: any;
try {
  pty = require('node-pty');
} catch {
  console.warn('node-pty not available — terminal will be disabled');
}

interface PtyInstance {
  id: number;
  process: any; // IPty
}

export class PtyManager {
  private instances = new Map<number, PtyInstance>();
  private nextId = 1;

  create(cwd: string): number | null {
    if (!pty) return null;

    const id = this.nextId++;
    const shell = process.env.SHELL || '/bin/zsh';

    const proc = pty.spawn(shell, [], {
      name: 'xterm-256color',
      cwd,
      cols: 80,
      rows: 24,
      env: {
        ...process.env,
        TERM: 'xterm-256color',
        COLORTERM: 'truecolor',
      },
    });

    this.instances.set(id, { id, process: proc });
    return id;
  }

  write(id: number, data: string): void {
    const inst = this.instances.get(id);
    if (inst) {
      inst.process.write(data);
    }
  }

  resize(id: number, cols: number, rows: number): void {
    const inst = this.instances.get(id);
    if (inst) {
      try {
        inst.process.resize(cols, rows);
      } catch {
        // resize can fail if the process has exited
      }
    }
  }

  destroy(id: number): void {
    const inst = this.instances.get(id);
    if (inst) {
      inst.process.kill();
      this.instances.delete(id);
    }
  }

  destroyAll(): void {
    for (const [id] of this.instances) {
      this.destroy(id);
    }
  }

  onData(id: number, callback: (data: string) => void): void {
    const inst = this.instances.get(id);
    if (inst) {
      inst.process.onData(callback);
    }
  }

  onExit(id: number, callback: (exitCode: number) => void): void {
    const inst = this.instances.get(id);
    if (inst) {
      inst.process.onExit(({ exitCode }: { exitCode: number }) => {
        callback(exitCode);
        this.instances.delete(id);
      });
    }
  }
}

export function registerPtyHandlers(manager: PtyManager, win: BrowserWindow): void {
  ipcMain.handle(IPC.TERMINAL_CREATE, async (_e, cwd: string) => {
    const id = manager.create(cwd);
    if (id === null) return -1; // signal that PTY is unavailable

    manager.onData(id, (data) => {
      if (!win.isDestroyed()) {
        win.webContents.send(IPC.TERMINAL_DATA, id, data);
      }
    });

    manager.onExit(id, (code) => {
      if (!win.isDestroyed()) {
        win.webContents.send(IPC.TERMINAL_EXIT, id, code);
      }
    });

    return id;
  });

  ipcMain.on(IPC.TERMINAL_WRITE, (_e, id: number, data: string) => {
    manager.write(id, data);
  });

  ipcMain.on(IPC.TERMINAL_RESIZE, (_e, id: number, cols: number, rows: number) => {
    manager.resize(id, cols, rows);
  });

  ipcMain.on(IPC.TERMINAL_DESTROY, (_e, id: number) => {
    manager.destroy(id);
  });
}
