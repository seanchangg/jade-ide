import { ipcMain, BrowserWindow } from 'electron';
import { spawn, ChildProcess } from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import { IPC, AiStatus, AiModelId, InfillRequest, InfillResult } from '../shared/types';

// AI inline completion backend — a llama.cpp server speaking the /infill
// (fill-in-the-middle) API. Resolution order:
//   1. JADE_FIM_ENDPOINT env var (user-managed server, any host)
//   2. http://127.0.0.1:8012 (llama.vscode convention — reuse if already running)
//   3. Spawn our own llama-server with a small FIM model (downloads on first run)

const DEFAULT_ENDPOINT = 'http://127.0.0.1:8012';
const MANAGED_PORT = 8630;

export const AI_MODELS: Record<AiModelId, { hf: string; label: string }> = {
  fast:     { hf: 'ggml-org/Qwen2.5-Coder-1.5B-Q8_0-GGUF', label: 'Qwen2.5-Coder 1.5B' },
  balanced: { hf: 'ggml-org/Qwen2.5-Coder-3B-Q8_0-GGUF',   label: 'Qwen2.5-Coder 3B' },
  best:     { hf: 'ggml-org/Qwen2.5-Coder-7B-Q8_0-GGUF',   label: 'Qwen2.5-Coder 7B' },
};
const HEALTH_TIMEOUT_MS = 800;
const STARTUP_DEADLINE_MS = 15 * 60_000; // generous: first run downloads the model
const REQUEST_TIMEOUT_MS = 4000;

// GUI apps on macOS don't inherit the shell PATH, so probe common install dirs.
const EXTRA_BIN_DIRS = ['/opt/homebrew/bin', '/usr/local/bin', '/opt/local/bin'];

export class InlineCompletionBackend {
  private proc: ChildProcess | null = null;
  private endpoint: string | null = null;
  private status: AiStatus = { state: 'disabled', detail: 'Not started' };
  private statusListeners: Array<(s: AiStatus) => void> = [];
  private inflight: AbortController | null = null;
  private stopped = false;
  private modelId: AiModelId = 'fast';

  async start(): Promise<void> {
    // Idempotent — a second call while running or mid-startup is a no-op
    if (this.proc || this.status.state === 'ready' || this.status.state === 'starting') return;
    this.stopped = false;

    const candidates = [process.env.JADE_FIM_ENDPOINT, DEFAULT_ENDPOINT]
      .filter((e): e is string => !!e);

    this.setStatus({ state: 'starting', detail: 'Looking for a completion server…' });

    for (const ep of candidates) {
      const healthy = await this.isHealthy(ep);
      if (this.stopped) return; // turned off while we were probing
      if (healthy) {
        this.endpoint = ep;
        this.setStatus({ state: 'ready', detail: `Connected to ${ep}`, endpoint: ep });
        return;
      }
    }

    const bin = this.findLlamaServer();
    if (!bin) {
      this.setStatus({
        state: 'disabled',
        detail: 'llama-server not found — `brew install llama.cpp`, or set JADE_FIM_ENDPOINT to a running server',
      });
      return;
    }

    this.spawnManaged(bin);
  }

  // Kill the model server and release its GPU/unified memory (e.g. while
  // benchmarking kernels). start() brings it back — the model is cached,
  // so a restart is a reload, not a re-download.
  stop(detail = 'Turned off'): void {
    this.stopped = true;
    this.inflight?.abort();
    if (this.proc) {
      this.proc.kill();
      this.proc = null;
    }
    this.endpoint = null;
    this.setStatus({ state: 'disabled', detail });
  }

  // Switch the managed model tier. Restarts the server if it's running our
  // model; external endpoints (JADE_FIM_ENDPOINT / 8012) pick their own model.
  async setModel(id: AiModelId): Promise<void> {
    if (!AI_MODELS[id] || id === this.modelId) return;
    this.modelId = id;

    const usingManaged = !!this.proc ||
      (this.endpoint !== null && this.endpoint.endsWith(`:${MANAGED_PORT}`));
    const running = this.status.state === 'ready' || this.status.state === 'starting';
    if (!running) return;               // off/errored — new tier applies on next start
    if (!usingManaged && this.status.state === 'ready') return;

    this.stop('Switching model…');
    await this.start();
  }

  getStatus(): AiStatus {
    return this.status;
  }

  onStatus(fn: (s: AiStatus) => void): void {
    this.statusListeners.push(fn);
  }

  async infill(req: InfillRequest): Promise<InfillResult | null> {
    if (!this.endpoint || this.status.state !== 'ready') return null;

    // Only the newest request matters — cancel whatever is still running so
    // the server's single slot isn't stuck generating a stale suggestion.
    this.inflight?.abort();
    const controller = new AbortController();
    this.inflight = controller;
    const timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);

    try {
      const res = await fetch(`${this.endpoint}/infill`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        signal: controller.signal,
        body: JSON.stringify({
          input_prefix: req.prefix,
          input_suffix: req.suffix,
          n_predict: req.singleLine ? 64 : 96,
          stop: req.singleLine ? ['\n'] : [],
          top_k: 40,
          top_p: 0.99,
          temperature: 0.1,
          cache_prompt: true,
          t_max_predict_ms: 1500,
        }),
      });

      if (!res.ok) return null;
      const json: any = await res.json();
      return typeof json.content === 'string' ? { content: json.content } : null;
    } catch {
      // Aborted (superseded/timeout) or server hiccup — no suggestion this round
      return null;
    } finally {
      clearTimeout(timeout);
      if (this.inflight === controller) this.inflight = null;
    }
  }

  shutdown(): void {
    this.stop('Shutting down');
  }

  private setStatus(s: AiStatus): void {
    this.status = s;
    for (const fn of this.statusListeners) fn(s);
  }

  private async isHealthy(endpoint: string): Promise<boolean> {
    try {
      const res = await fetch(`${endpoint}/health`, {
        signal: AbortSignal.timeout(HEALTH_TIMEOUT_MS),
      });
      return res.ok;
    } catch {
      return false;
    }
  }

  private findLlamaServer(): string | null {
    if (process.env.JADE_LLAMA_SERVER && fs.existsSync(process.env.JADE_LLAMA_SERVER)) {
      return process.env.JADE_LLAMA_SERVER;
    }
    const pathDirs = (process.env.PATH || '').split(path.delimiter);
    for (const dir of [...pathDirs, ...EXTRA_BIN_DIRS]) {
      if (!dir) continue;
      const candidate = path.join(dir, 'llama-server');
      try {
        fs.accessSync(candidate, fs.constants.X_OK);
        return candidate;
      } catch {}
    }
    return null;
  }

  private spawnManaged(bin: string): void {
    const endpoint = `http://127.0.0.1:${MANAGED_PORT}`;
    const model = AI_MODELS[this.modelId];
    this.setStatus({
      state: 'starting',
      detail: `Starting ${model.label} (downloads on first use)…`,
    });

    this.proc = spawn(bin, [
      '-hf', model.hf,
      '--host', '127.0.0.1',
      '--port', String(MANAGED_PORT),
      '-ngl', '99',           // full GPU offload (Metal)
      '-ub', '1024',
      '-b', '1024',
      '--ctx-size', '0',      // use the model's full context window
      '--cache-reuse', '256', // KV-prefix reuse — the JetBrains-style cache trick
    ], { stdio: ['ignore', 'ignore', 'pipe'] });

    let stderrTail = '';
    this.proc.stderr?.on('data', (data: Buffer) => {
      stderrTail = (stderrTail + data.toString()).slice(-2000);
    });

    this.proc.on('error', (err) => {
      this.setStatus({ state: 'error', detail: `Failed to launch llama-server: ${err.message}` });
      this.proc = null;
    });

    this.proc.on('exit', (code) => {
      this.proc = null;
      if (!this.stopped && this.status.state !== 'ready') {
        this.setStatus({
          state: 'error',
          detail: `llama-server exited with code ${code}: ${stderrTail.split('\n').slice(-3).join(' ')}`,
        });
      }
    });

    // Poll until the model is loaded (server returns 503 while loading)
    const deadline = Date.now() + STARTUP_DEADLINE_MS;
    const poll = async (): Promise<void> => {
      if (this.stopped || !this.proc) return;
      if (await this.isHealthy(endpoint)) {
        this.endpoint = endpoint;
        this.setStatus({ state: 'ready', detail: `Local model ready (${model.label})`, endpoint });
        return;
      }
      if (Date.now() > deadline) {
        this.setStatus({ state: 'error', detail: 'Timed out waiting for the completion model to load' });
        this.shutdown();
        return;
      }
      setTimeout(poll, 1500);
    };
    setTimeout(poll, 1500);
  }
}

export function registerAiHandlers(backend: InlineCompletionBackend, win: BrowserWindow): void {
  ipcMain.handle(IPC.AI_INFILL, async (_e, req: InfillRequest) => {
    return backend.infill(req);
  });

  ipcMain.handle(IPC.AI_STATUS, async () => {
    return backend.getStatus();
  });

  ipcMain.handle(IPC.AI_SET_ENABLED, async (_e, enabled: boolean) => {
    if (enabled) await backend.start();
    else backend.stop();
  });

  ipcMain.handle(IPC.AI_SET_MODEL, async (_e, id: AiModelId) => {
    await backend.setModel(id);
  });

  backend.onStatus((status) => {
    if (!win.isDestroyed()) {
      win.webContents.send(IPC.AI_STATUS_CHANGED, status);
    }
  });
}
