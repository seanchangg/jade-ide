import type { StickyNote, TabState, SystemStats, MemoryBarData, AllocationEvent, TrainingScalar, TrainingTimingEvent, MemoryAnnotation } from '../shared/types';

// Lightweight reactive state — no framework dependency.
// Components subscribe to keys they care about. Updates are synchronous.

type Listener = (value: any) => void;

class Store {
  private data: Record<string, any> = {};
  private listeners = new Map<string, Set<Listener>>();

  get<T>(key: string): T | undefined {
    return this.data[key] as T;
  }

  set(key: string, value: any): void {
    this.data[key] = value;
    const subs = this.listeners.get(key);
    if (subs) {
      for (const fn of subs) fn(value);
    }
  }

  on(key: string, fn: Listener): () => void {
    if (!this.listeners.has(key)) {
      this.listeners.set(key, new Set());
    }
    this.listeners.get(key)!.add(fn);
    return () => this.listeners.get(key)?.delete(fn);
  }

  // Update a value with a mutator (avoids get/set boilerplate)
  update<T>(key: string, fn: (current: T) => T): void {
    this.set(key, fn(this.data[key] as T));
  }
}

export const store = new Store();

// ── State keys and initial values ──

export function initializeState(): void {
  // UI panels
  store.set('fileTreeVisible', false);
  store.set('terminalVisible', false);
  store.set('memoryBarVisible', true);
  store.set('executionFlowVisible', false);
  store.set('trainingViewVisible', false);
  store.set('runtimeVisible', false);
  store.set('fileTreeWidth', 260);
  store.set('terminalHeight', 280);

  // Editor state
  store.set('openTabs', [] as TabState[]);
  store.set('activeTabIndex', -1);
  store.set('activeFilePath', '');

  // Workspace
  store.set('workspaceRoot', '');
  store.set('workspaceReady', false);

  // LSP
  store.set('lspReady', false);

  // AI inline completion
  store.set('aiStatus', 'disabled');
  store.set('aiStatusDetail', '');
  store.set('aiCompletionEnabled', true);
  store.set('aiMultiline', false); // single-line ghost text by default (CLion-style)
  store.set('aiModel', 'fast');

  // Data
  store.set('stickyNotes', [] as StickyNote[]);
  store.set('systemStats', { cpuPercent: 0, gpuPercent: -1, gpuName: '', memTotal: 0, memUsed: 0 } as SystemStats);
  store.set('memoryBar', { heapUsed: 0, stackSize: 0, peakAllocation: 0, pressurePercent: 0, allocCount: 0, freeCount: 0, leakCount: 0 } as MemoryBarData);
  store.set('memoryAnnotations', new Map<string, MemoryAnnotation[]>());
  store.set('allocationEvents', [] as AllocationEvent[]);
  store.set('executedLines', new Map<number, number>());
  store.set('trainingScalars', new Map<string, TrainingScalar[]>());
  store.set('trainingTimings', [] as TrainingTimingEvent[]);

  // Build state
  store.set('isBuilding', false);
  store.set('isRunning', false);
  store.set('lastBuildResult', null);

  // Debug state
  store.set('debugState', 'idle');
  store.set('benchmarks', []);
  store.set('debugPausedLine', null);
  store.set('breakpoints', {} as Record<string, number[]>);
}

// ── Persistence helpers ──

const PERSISTED_KEYS = [
  'fileTreeVisible', 'terminalVisible', 'memoryBarVisible',
  'fileTreeWidth', 'terminalHeight', 'stickyNotes',
  'openTabs', 'activeTabIndex', 'breakpoints', 'benchmarks',
  'aiCompletionEnabled',
];

export async function savePersistedState(): Promise<void> {
  const state: Record<string, any> = {};
  for (const key of PERSISTED_KEYS) {
    const val = store.get(key);
    // Convert Sets/Maps to serializable form
    if (val instanceof Set) {
      state[key] = Array.from(val);
    } else if (val instanceof Map) {
      state[key] = Object.fromEntries(val);
    } else {
      state[key] = val;
    }
  }
  await (window as any).forge.state.save('ui', state);
}

export async function loadPersistedState(): Promise<void> {
  const state = await (window as any).forge.state.load('ui');
  if (!state) return;

  for (const key of PERSISTED_KEYS) {
    if (key in state) {
      store.set(key, state[key]);
    }
  }
}

// Auto-save on changes (debounced)
let saveTimer: ReturnType<typeof setTimeout> | null = null;
export function enableAutoSave(): void {
  for (const key of PERSISTED_KEYS) {
    store.on(key, () => {
      if (saveTimer) clearTimeout(saveTimer);
      saveTimer = setTimeout(savePersistedState, 1500);
    });
  }
}
