import { ipcMain, BrowserWindow } from 'electron';
import { spawn, ChildProcess } from 'child_process';
import * as path from 'path';
import { IPC, Breakpoint, StackFrame, LocalVariable, DebugStoppedEvent } from '../shared/types';
import { ensureProbeDylib, FORGE_PROBE_DYLIB } from './build-runner';
import { getTelemetryServer } from './telemetry-server';

const PROMPT = 'FORGE_LLDB> ';
const PROMPT_RE = /FORGE_LLDB> $/;

class LldbDriver {
  private proc: ChildProcess | null = null;
  private win: BrowserWindow;
  private buffer = '';
  private pendingResolve: ((output: string) => void) | null = null;
  private ready = false;
  // Serializes sendCommand calls — concurrent commands would clobber
  // pendingResolve and pair responses with the wrong promise.
  private cmdQueue: Promise<unknown> = Promise.resolve();
  // lldb over a pipe never prints a trailing prompt after a command's output
  // (the prompt only appears fused with the NEXT command's echo), so command
  // responses are delimited by a quiet period instead.
  private pendingSettle: ReturnType<typeof setTimeout> | null = null;
  // Settle timer for unsolicited output (async breakpoint hits / process exit).
  private asyncTimer: ReturnType<typeof setTimeout> | null = null;
  private exitSent = false;

  constructor(win: BrowserWindow) {
    this.win = win;
  }

  async start(executable: string, cwd: string, breakpoints: Breakpoint[]): Promise<void> {
    if (this.proc) await this.stop();

    this.buffer = '';
    this.ready = false;
    this.exitSent = false;

    this.proc = spawn('lldb', [executable], {
      cwd,
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    this.proc.stdout!.on('data', (data: Buffer) => {
      this.onData(data.toString());
    });

    this.proc.stderr!.on('data', (data: Buffer) => {
      // Forward stderr as debug output
      this.send(IPC.DEBUG_OUTPUT, data.toString());
    });

    this.proc.on('close', (code) => {
      this.proc = null;
      this.ready = false;
      if (!this.exitSent) {
        this.exitSent = true;
        this.send(IPC.DEBUG_EXITED, code ?? -1);
      }
    });

    this.proc.on('error', (err) => {
      this.send(IPC.DEBUG_OUTPUT, `Failed to start lldb: ${err.message}\n`);
      this.proc = null;
    });

    // Wait for initial prompt
    await this.waitForPrompt();

    // Configure LLDB for machine-parseable output
    await this.sendCommand(`settings set prompt "${PROMPT}"`);
    await this.sendCommand('settings set auto-confirm true');
    await this.sendCommand('settings set stop-line-count-before 0');
    await this.sendCommand('settings set stop-line-count-after 0');

    // Give the debuggee the same telemetry environment as a normal run: the
    // telemetry socket plus the Metal probe dylib, so GPU buffers / scalars /
    // timings stream to the sidebar (and 3D weight grid) during debugging too.
    const envVars: string[] = [];
    const sockPath = getTelemetryServer()?.socketPath;
    if (sockPath) envVars.push(`FORGE_TELEMETRY_SOCK=${sockPath}`);
    if (ensureProbeDylib()) envVars.push(`DYLD_INSERT_LIBRARIES=${FORGE_PROBE_DYLIB}`);
    if (envVars.length > 0) {
      await this.sendCommand(`settings set target.env-vars ${envVars.join(' ')}`);
    }

    // Set all breakpoints
    for (const bp of breakpoints) {
      await this.sendCommand(`breakpoint set --file "${bp.file}" --line ${bp.line}`);
    }

    // Run the program
    this.ready = true;
    const runOutput = await this.sendCommand('run');

    // Check if we hit a breakpoint immediately
    await this.handleStopIfNeeded(runOutput);
  }

  async stop(): Promise<void> {
    if (!this.proc) return;
    try {
      this.proc.stdin!.write('kill\n');
      this.proc.stdin!.write('quit\n');
    } catch {}
    setTimeout(() => {
      if (this.proc) {
        this.proc.kill('SIGKILL');
        this.proc = null;
      }
    }, 1000);
  }

  async setBreakpoint(file: string, line: number): Promise<void> {
    if (!this.proc || !this.ready) return;
    await this.sendCommand(`breakpoint set --file "${file}" --line ${line}`);
  }

  async removeBreakpoint(file: string, line: number): Promise<void> {
    if (!this.proc || !this.ready) return;
    // List breakpoints and find matching one
    const output = await this.sendCommand('breakpoint list');
    const lines = output.split('\n');
    for (const l of lines) {
      // Match: "N: file = 'path', line = N"
      const idMatch = l.match(/^(\d+):/);
      if (!idMatch) continue;
      const id = idMatch[1];
      if (l.includes(path.basename(file)) && l.includes(`line = ${line}`)) {
        await this.sendCommand(`breakpoint delete ${id}`);
        return;
      }
    }
  }

  async continue_(): Promise<void> {
    if (!this.proc || !this.ready) return;
    const output = await this.sendCommand('continue');
    await this.handleStopIfNeeded(output);
  }

  async stepOver(): Promise<void> {
    if (!this.proc || !this.ready) return;
    const output = await this.sendCommand('next');
    await this.handleStopIfNeeded(output);
  }

  async stepInto(): Promise<void> {
    if (!this.proc || !this.ready) return;
    const output = await this.sendCommand('step');
    await this.handleStopIfNeeded(output);
  }

  async stepOut(): Promise<void> {
    if (!this.proc || !this.ready) return;
    const output = await this.sendCommand('finish');
    await this.handleStopIfNeeded(output);
  }

  // ── Internal ──

  private send(channel: string, data: any): void {
    if (!this.win.isDestroyed()) {
      this.win.webContents.send(channel, data);
    }
  }

  private onData(text: string): void {
    this.buffer += text;
    this.send(IPC.DEBUG_OUTPUT, text);

    if (this.pendingResolve) {
      // Fast path: a trailing prompt definitely ends the response.
      if (PROMPT_RE.test(this.buffer)) {
        this.resolvePending();
        return;
      }
      // Otherwise the response is done when output goes quiet.
      if (this.pendingSettle) clearTimeout(this.pendingSettle);
      this.pendingSettle = setTimeout(() => this.resolvePending(), 150);
      return;
    }

    // No command in flight: this is unsolicited output. Breakpoint hits and
    // process exit arrive here — lldb reports them asynchronously, well after
    // run/continue returned. Give the chunk a moment to complete, then inspect.
    if (this.asyncTimer) clearTimeout(this.asyncTimer);
    this.asyncTimer = setTimeout(() => {
      this.asyncTimer = null;
      this.handleAsyncOutput();
    }, 80);
  }

  private resolvePending(): void {
    if (!this.pendingResolve) return;
    if (this.pendingSettle) {
      clearTimeout(this.pendingSettle);
      this.pendingSettle = null;
    }
    const output = this.buffer;
    this.buffer = '';
    const resolve = this.pendingResolve;
    this.pendingResolve = null;
    resolve(output);
  }

  // Inspect unsolicited lldb output for stop / exit events.
  private handleAsyncOutput(): void {
    if (this.pendingResolve || !this.proc) return;
    const output = this.buffer;
    if (/stop reason = |exited with status/.test(output)) {
      this.buffer = '';
      void this.handleStopIfNeeded(output);
    }
  }

  private waitForPrompt(): Promise<string> {
    return new Promise((resolve) => {
      // Check if buffer already has a prompt (initial or custom)
      const check = () => {
        if (this.buffer.includes('(lldb)') || PROMPT_RE.test(this.buffer)) {
          const output = this.buffer;
          this.buffer = '';
          resolve(output);
        } else {
          this.pendingResolve = resolve;
        }
      };
      // Give lldb a moment to start
      setTimeout(check, 100);
    });
  }

  // Queued so concurrent callers can't clobber each other's pendingResolve —
  // responses always pair with the command that produced them.
  private sendCommand(cmd: string): Promise<string> {
    const run = (): Promise<string> =>
      new Promise((resolve) => {
        if (!this.proc || !this.proc.stdin) {
          resolve('');
          return;
        }
        if (this.asyncTimer) {
          clearTimeout(this.asyncTimer);
          this.asyncTimer = null;
        }
        this.pendingResolve = resolve;
        this.buffer = '';
        this.proc.stdin.write(cmd + '\n');

        // Timeout after 10s
        setTimeout(() => {
          if (this.pendingResolve === resolve) {
            this.pendingResolve = null;
            resolve(this.buffer);
            this.buffer = '';
          }
        }, 10000);
      });

    const queued = this.cmdQueue.then(run, run);
    this.cmdQueue = queued.catch(() => {});
    return queued;
  }

  private async handleStopIfNeeded(output: string): Promise<void> {
    // Process exit ends the session — report it and shut lldb down.
    const exitMatch = output.match(/Process \d+ exited with status = (-?\d+)/);
    if (exitMatch) {
      if (!this.exitSent) {
        this.exitSent = true;
        this.send(IPC.DEBUG_EXITED, parseInt(exitMatch[1], 10));
      }
      try {
        this.proc?.stdin?.write('quit\n');
      } catch {}
      return;
    }

    // Check if process stopped at a breakpoint or step
    const stopMatch = output.match(/stop reason = (.+)/);
    if (!stopMatch) return;

    const reason = stopMatch[1].trim();

    // Parse current location from "frame #0:" or "at file:line"
    let file = '';
    let line = 0;

    const locMatch = output.match(/at\s+(\S+?):(\d+)/);
    if (locMatch) {
      file = locMatch[1];
      line = parseInt(locMatch[2], 10);
    }

    // Get locals and call stack (sequential — lldb answers one command at a time).
    // -T annotates types at every nesting depth so pointers stay expandable.
    const localsOutput = await this.sendCommand('frame variable -T');
    const btOutput = await this.sendCommand('bt');

    const locals = this.parseVariables(localsOutput);
    const frames = this.parseBacktrace(btOutput);

    // Use frame info for file if location match failed
    if (!file && frames.length > 0) {
      file = frames[0].file;
      line = frames[0].line;
    }

    const event: DebugStoppedEvent = { reason, file, line, frames, locals };
    this.send(IPC.DEBUG_STOPPED, event);
  }

  // Parse lldb `frame variable` output into a tree. Handles:
  //   (int) plain = 42                          — leaf
  //   (Matrix) m = {                            — aggregate block, indented
  //     rows = 128                              —   members (no type prefix)
  //     inner = (q = 7, w = 9.5)                —   inline aggregate
  //   }
  //   (std::vector<int>) vec = size=3 { [0]=10… — sized container
  //   (float *) buf = 0x16fdfe7d0               — pointer (lazily expandable)
  private parseVariables(output: string, rootPath?: string): LocalVariable[] {
    const roots: LocalVariable[] = [];
    const stack: Array<{ node: LocalVariable; indent: number }> = [];

    for (const raw of output.split('\n')) {
      const line = raw.replace(/\r$/, '');
      const trimmed = line.trim();
      if (trimmed === '}' || trimmed === '},') {
        if (stack.length) stack.pop();
        continue;
      }

      const m = line.match(/^(\s*)(?:\(([^=]*?)\)\s+)?([^=(){}]+?)\s*=\s*(.*)$/);
      if (!m) continue;

      const indent = m[1].length;
      while (stack.length && indent <= stack[stack.length - 1].indent) stack.pop();
      const parent = stack.length ? stack[stack.length - 1].node : null;

      // Members inside a block have no "(type)" prefix; top-level entries do.
      // A prefix-less line at indent 0 is lldb noise, not a variable.
      if (!parent && !m[2]) continue;

      const name = m[3].trim();
      let value = m[4].trim();
      const opensBlock = /\{$/.test(value);
      if (opensBlock) value = value.replace(/\{$/, '').trim();

      let path: string;
      if (!parent) path = rootPath || name;
      else if (name.startsWith('[')) path = `${parent.path}${name}`;
      else if (name.startsWith('*')) path = `*${parent.path}`;
      else path = `${parent.path}.${name}`;

      const node: LocalVariable = {
        type: m[2]?.trim() || '',
        name,
        value: opensBlock ? (value ? `${value} {…}` : '{…}') : value,
        path,
      };
      // Pointers expand lazily via DEBUG_VAR_CHILDREN (skip c-strings — the
      // string is already in the value, and null pointers — nothing behind them)
      if (/\*/.test(node.type) && /^0x[0-9a-fA-F]+$/.test(node.value) && !/^0x0+$/.test(node.value)) {
        node.expandable = true;
      }
      if (opensBlock) {
        node.children = [];
        node.expandable = true;
      }

      (parent ? parent.children! : roots).push(node);
      if (opensBlock) stack.push({ node, indent });
    }
    return roots;
  }

  // Fetch one level of members behind a pointer / expression path.
  async getVarChildren(expr: string): Promise<LocalVariable[]> {
    if (!this.proc || !this.ready) return [];
    const output = await this.sendCommand(`frame variable -T -P 1 -- ${expr}`);
    const roots = this.parseVariables(output, expr);
    const root = roots[0];
    if (!root) return [];
    return root.children || [];
  }

  private parseBacktrace(output: string): StackFrame[] {
    const frames: StackFrame[] = [];
    const lines = output.split('\n');
    for (const l of lines) {
      // Format: "frame #N: 0xADDR module`func at file:line:col"
      // or: "* frame #N: 0xADDR module`func at file:line"
      const m = l.match(/frame #(\d+):.+?`(.+?)\s+at\s+(\S+?):(\d+)/);
      if (m) {
        frames.push({
          index: parseInt(m[1], 10),
          functionName: m[2],
          file: m[3],
          line: parseInt(m[4], 10),
        });
      }
    }
    return frames;
  }
}

export function registerDebugHandlers(win: BrowserWindow): void {
  const driver = new LldbDriver(win);

  ipcMain.handle(IPC.DEBUG_START, async (_e, executable: string, cwd: string, breakpoints: Breakpoint[]) => {
    return driver.start(executable, cwd, breakpoints);
  });

  ipcMain.handle(IPC.DEBUG_STOP, async () => driver.stop());
  ipcMain.handle(IPC.DEBUG_CONTINUE, async () => driver.continue_());
  ipcMain.handle(IPC.DEBUG_STEP_OVER, async () => driver.stepOver());
  ipcMain.handle(IPC.DEBUG_STEP_INTO, async () => driver.stepInto());
  ipcMain.handle(IPC.DEBUG_STEP_OUT, async () => driver.stepOut());

  ipcMain.handle(IPC.DEBUG_SET_BREAKPOINT, async (_e, file: string, line: number) => {
    return driver.setBreakpoint(file, line);
  });

  ipcMain.handle(IPC.DEBUG_REMOVE_BREAKPOINT, async (_e, file: string, line: number) => {
    return driver.removeBreakpoint(file, line);
  });

  ipcMain.handle(IPC.DEBUG_VAR_CHILDREN, async (_e, expr: string) => {
    return driver.getVarChildren(expr);
  });
}
