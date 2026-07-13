// ── File system ──

export interface FileNode {
  name: string;
  path: string;
  isDirectory: boolean;
  children?: FileNode[];
}

export interface FileChangeEvent {
  type: 'create' | 'change' | 'delete';
  path: string;
}

// ── Editor ──

export interface OpenFileResult {
  path: string;
  content: string;
  language: string;
}

export interface TabState {
  path: string;
  viewState?: any; // Monaco ICodeEditorViewState
  isDirty: boolean;
}

// ── LSP ──

export interface Position {
  line: number;
  character: number;
}

export interface Range {
  start: Position;
  end: Position;
}

export interface DiagnosticData {
  uri: string;
  diagnostics: Array<{
    range: Range;
    message: string;
    severity: number; // 1=error, 2=warning, 3=info, 4=hint
    source?: string;
  }>;
}

export interface CompletionData {
  label: string;
  kind: number;
  detail?: string;
  documentation?: string;
  insertText: string;
  sortText?: string;
}

export interface HoverData {
  contents: string;
  range?: Range;
}

export interface LocationData {
  uri: string;
  range: Range;
}

// ── Memory analysis ──

export interface MemoryAnnotation {
  line: number;
  typeName?: string;
  staticSize?: number;       // sizeof from clangd
  runtimeAllocCount?: number; // calls observed at this line
  runtimeTotalBytes?: number; // total bytes allocated at this line
  runtimeLeakCount?: number;  // unfreed allocations from this line
}

export interface MemoryBarData {
  heapUsed: number;
  stackSize: number;
  peakAllocation: number;
  pressurePercent: number;
  allocCount: number;
  freeCount: number;
  leakCount: number;
}

export interface AllocationEvent {
  type: 'alloc' | 'free';
  pointer: string;
  size: number;
  file: string;
  line: number;
  timestamp: number;
}

// ── Execution flow ──

export interface ExecutionNode {
  file: string;
  line: number;
  type: 'call' | 'return' | 'branch' | 'loop-start' | 'loop-back' | 'error';
  functionName?: string;
  children?: ExecutionNode[];
}

export interface ExecutionTrace {
  nodes: ExecutionNode[];
  executedLines: Map<number, number>;  // line -> execution count
  errorLine?: number;
}

// ── Sticky notes ──

export interface StickyNote {
  id: string;
  filePath: string;
  anchorLine: number;
  x: number;
  y: number;
  width: number;
  height: number;
  content: string; // markdown, supports [ ] and [x] checkboxes
  createdAt: number;
  updatedAt: number;
}

// ── System stats ──

export interface SystemStats {
  cpuPercent: number;
  gpuPercent: number;    // -1 if unavailable
  gpuName: string;
  memTotal: number;
  memUsed: number;
}

// ── Build / Run ──

export interface BuildResult {
  success: boolean;
  executable?: string;
  errors: Array<{
    file: string;
    line: number;
    column: number;
    message: string;
    severity: 'error' | 'warning' | 'note';
  }>;
  duration: number;
}

export interface RunConfig {
  executable: string;
  args?: string[];
  env?: Record<string, string>;
  enableSanitizers?: boolean;
  enableInstrumentation?: boolean; // -finstrument-functions
}

export interface RunResult {
  exitCode: number;
  duration: number;
  memoryTrace?: AllocationEvent[];
  executedLines?: Record<string, number>;  // serialized Map: lineNum -> count
  peakMemory?: number;
  sanitizerOutput?: string;
}

// ── ML Training ──

export interface TrainingScalar {
  name: string;
  step: number;
  value: number;
  timestamp: number;
}

export interface TrainingTimingEvent {
  name: string;
  durationMs: number;
  step: number;
}

// ── Telemetry (auto-discovery + control channel) ──

export type TelemetryKind = 'scalar' | 'timer' | 'buffer';

export interface TelemetryMeta {
  rows?: number;
  cols?: number;
  dtype?: string;
  label?: string;
}

// Discovery notification: main → renderer when a new name is seen
// (via socket decl, a socket data message, or legacy __FORGE_* stdout).
export interface TelemetryDecl {
  kind: TelemetryKind;
  name: string;
  meta?: TelemetryMeta;
  // Set when a client renames an already-declared item (e.g. a Metal buffer
  // declared at allocation as "buffer#3" then given the label "model.weights").
  // The registry migrates the existing entry — and its checkbox/enabled state —
  // instead of leaving a stale row under the old name.
  renamedFrom?: string;
}

// A single GPU-buffer / tensor frame streamed by a probe client.
// `data` is base64 of a row-major float32 array (already downsampled
// to <= maxDim x maxDim by the client).
export interface TelemetryTensor {
  name: string;
  step: number;
  rows: number;
  cols: number;
  // Pre-pooling source dimensions (for axis labeling); may equal rows/cols.
  srcRows?: number;
  srcCols?: number;
  dtype: string;
  data: string;
}

// Control message: renderer → main → socket clients. Sent when a
// checkbox toggles and replayed to newly connected clients for every
// currently-enabled item.
export interface TelemetryTrack {
  kind: TelemetryKind;
  name: string;
  enabled: boolean;
  maxDim: number;
  // Optional user-provided tensor shape hint for buffers.
  rows?: number;
  cols?: number;
}

// ── Debug ──

export interface Breakpoint {
  file: string;
  line: number;
}

export interface StackFrame {
  index: number;
  functionName: string;
  file: string;
  line: number;
}

export interface LocalVariable {
  name: string;
  type: string;
  value: string;
  /** LLDB expression path (e.g. "m.inner.q", "vec[0]", "*m.buf") for lazy expansion */
  path?: string;
  /** Nested members parsed inline from lldb output */
  children?: LocalVariable[];
  /** True when children exist or can be fetched (pointers) */
  expandable?: boolean;
}

export type DebugState = 'idle' | 'running' | 'paused';

export interface DebugStoppedEvent {
  reason: string;
  file: string;
  line: number;
  frames: StackFrame[];
  locals: LocalVariable[];
}

// ── AI inline completion (fill-in-the-middle) ──

export type AiCompletionState = 'disabled' | 'starting' | 'ready' | 'error';

// Managed-model tiers — all FIM-tuned Qwen2.5-Coder GGUFs served by llama.cpp
export type AiModelId = 'fast' | 'balanced' | 'best';

export interface AiStatus {
  state: AiCompletionState;
  detail: string;
  endpoint?: string;
}

export interface InfillRequest {
  prefix: string;
  suffix: string;
  filename?: string;
  // Single-line mode: server stops generating at the first newline —
  // lower latency and CLion-style one-line ghost text
  singleLine?: boolean;
}

export interface InfillResult {
  content: string;
}

// ── Workspace state (persisted to JSON) ──

export interface WorkspaceState {
  rootPath: string;
  openTabs: TabState[];
  activeTabIndex: number;
  stickyNotes: StickyNote[];
  fileTreeWidth: number;
  terminalHeight: number;
  fileTreeVisible: boolean;
  terminalVisible: boolean;
  memoryBarVisible: boolean;
  executionFlowVisible: boolean;
}

// ── IPC channel names ──

export const IPC = {
  // Workspace
  OPEN_FOLDER_DIALOG: 'workspace:open-folder-dialog',
  OPEN_WORKSPACE: 'workspace:open',
  GET_FILE_TREE: 'workspace:get-tree',
  READ_FILE: 'workspace:read-file',
  WRITE_FILE: 'workspace:write-file',
  RENAME_FILE: 'workspace:rename',
  DELETE_FILE: 'workspace:delete',
  CREATE_FILE: 'workspace:create-file',
  CREATE_DIR: 'workspace:create-dir',
  FILE_CHANGED: 'workspace:file-changed',

  // LSP
  LSP_INITIALIZE: 'lsp:initialize',
  LSP_COMPLETION: 'lsp:completion',
  LSP_HOVER: 'lsp:hover',
  LSP_DEFINITION: 'lsp:definition',
  LSP_REFERENCES: 'lsp:references',
  LSP_DIAGNOSTICS: 'lsp:diagnostics',
  LSP_DID_OPEN: 'lsp:did-open',
  LSP_DID_CHANGE: 'lsp:did-change',
  LSP_DID_CLOSE: 'lsp:did-close',
  LSP_MEMORY_INFO: 'lsp:memory-info',

  // Terminal
  TERMINAL_CREATE: 'terminal:create',
  TERMINAL_WRITE: 'terminal:write',
  TERMINAL_RESIZE: 'terminal:resize',
  TERMINAL_DATA: 'terminal:data',
  TERMINAL_DESTROY: 'terminal:destroy',
  TERMINAL_EXIT: 'terminal:exit',

  // System monitoring
  SYSTEM_STATS: 'system:stats',
  SYSTEM_START: 'system:start',
  SYSTEM_STOP: 'system:stop',

  // Build / Run
  BUILD_COMPILE: 'build:compile',
  BUILD_RUN: 'build:run',
  BUILD_OUTPUT: 'build:output',
  BUILD_STOP: 'build:stop',
  BUILD_MEMORY_EVENT: 'build:memory-event',
  BUILD_TRAINING_SCALAR: 'build:training-scalar',
  BUILD_TRAINING_TIMING: 'build:training-timing',

  // Telemetry (auto-discovery + control channel)
  TELEMETRY_DECL: 'telemetry:decl',       // main → renderer: new item discovered
  TELEMETRY_TENSOR: 'telemetry:tensor',   // main → renderer: a tracked buffer frame
  TELEMETRY_TRACK: 'telemetry:track',     // renderer → main: toggle tracking

  // Assembly
  BUILD_ASM: 'build:asm',

  // Debug
  DEBUG_START: 'debug:start',
  DEBUG_STOP: 'debug:stop',
  DEBUG_CONTINUE: 'debug:continue',
  DEBUG_STEP_OVER: 'debug:step-over',
  DEBUG_STEP_INTO: 'debug:step-into',
  DEBUG_STEP_OUT: 'debug:step-out',
  DEBUG_SET_BREAKPOINT: 'debug:set-breakpoint',
  DEBUG_REMOVE_BREAKPOINT: 'debug:remove-breakpoint',
  DEBUG_VAR_CHILDREN: 'debug:var-children',
  DEBUG_STOPPED: 'debug:stopped',
  DEBUG_OUTPUT: 'debug:output',
  DEBUG_EXITED: 'debug:exited',

  // AI inline completion
  AI_INFILL: 'ai:infill',
  AI_STATUS: 'ai:status',
  AI_STATUS_CHANGED: 'ai:status-changed',
  AI_SET_ENABLED: 'ai:set-enabled',
  AI_SET_MODEL: 'ai:set-model',

  // State persistence
  STATE_SAVE: 'state:save',
  STATE_LOAD: 'state:load',
} as const;
