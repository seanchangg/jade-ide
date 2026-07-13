import * as monaco from 'monaco-editor';
import { store } from '../state';
import type { MemoryAnnotation, AllocationEvent, MemoryBarData } from '../../shared/types';

const forge = (window as any).forge;

// Decoration IDs for runtime allocation annotations
let decorationIds: string[] = [];

// Annotation generation — each scan gets a unique generation so old decorations
// become invisible when their CSS rules are cleared, even if deltaDecorations
// fails to remove them from the model.
let annotationGeneration = 0;

// Style element for annotation ::after rules
let annotationStyleEl: HTMLStyleElement | null = null;

// Previous execution data for diff-aware annotations (Feature 5)
let previousExecutedLines: Map<number, number> = new Map();

// Track allocations by file:line
const allocationTracker = new Map<string, {
  allocCount: number;
  totalBytes: number;
  leakCount: number;
  pointers: Map<string, number>; // pointer -> size (unfreed)
}>();

// Global pointer -> tracker-key index so free events are O(1) instead of
// scanning every tracker on each free (allocs can arrive thousands/sec).
const pointerIndex = new Map<string, string>();

// Accumulate memory bar totals
let memBar: MemoryBarData = {
  heapUsed: 0, stackSize: 0, peakAllocation: 0,
  pressurePercent: 0, allocCount: 0, freeCount: 0, leakCount: 0,
};

// ── Known C++ type sizes (LP64 / Apple Silicon) ──
const PRIMITIVE_SIZES: Record<string, number> = {
  'bool': 1,
  'char': 1, 'signed char': 1, 'unsigned char': 1,
  'int8_t': 1, 'uint8_t': 1,
  'short': 2, 'unsigned short': 2,
  'int16_t': 2, 'uint16_t': 2,
  'int': 4, 'unsigned int': 4, 'unsigned': 4,
  'int32_t': 4, 'uint32_t': 4,
  'float': 4,
  'long': 8, 'unsigned long': 8,
  'long long': 8, 'unsigned long long': 8,
  'int64_t': 8, 'uint64_t': 8,
  'double': 8,
  'long double': 16,
  'size_t': 8, 'ssize_t': 8, 'ptrdiff_t': 8,
  'void*': 8, 'nullptr_t': 8,
};

// Common STL container sizes (approximate, varies by implementation)
const STL_SIZES: Record<string, number> = {
  'std::string': 24,
  'std::string_view': 16,
  'std::vector': 24,
  'std::array': -1,           // needs element size * count
  'std::map': 48,
  'std::unordered_map': 56,
  'std::set': 48,
  'std::unordered_set': 56,
  'std::list': 24,
  'std::deque': 80,
  'std::queue': 80,
  'std::stack': 80,
  'std::pair': -1,            // sum of both types
  'std::tuple': -1,
  'std::optional': -1,        // sizeof(T) + 1 (padded)
  'std::unique_ptr': 8,
  'std::shared_ptr': 16,
  'std::weak_ptr': 16,
  'std::function': 32,
  'std::mutex': 64,
  'std::atomic': -1,
  'std::thread': 8,
  'std::chrono::steady_clock::time_point': 8,
  'std::chrono::system_clock::time_point': 8,
};

export function initMemoryDecorations(editor: monaco.editor.IStandaloneCodeEditor): void {
  // Listen for ALL memory events — idetools.h protocol, ASan, and malloc interposer
  forge.build.onMemoryEvent((event: any) => {
    if (event.type === 'alloc' || event.type === 'free') {
      // idetools.h protocol
      processAllocationEvent(event as AllocationEvent);
    } else if (event.type === 'heap-summary') {
      // Malloc interposer summary at exit
      memBar.heapUsed = event.currentHeap;
      memBar.peakAllocation = event.peakHeap;
      memBar.allocCount = event.allocCount;
      memBar.freeCount = event.freeCount;
      memBar.leakCount = event.allocCount - event.freeCount;
    } else if (event.type === 'asan-stats') {
      // ASan allocation stats
      memBar.allocCount = event.totalAllocations || memBar.allocCount;
      memBar.freeCount = event.totalFrees || memBar.freeCount;
      if (event.totalBytes) memBar.heapUsed = event.totalBytes - (event.totalFreedBytes || 0);
      if (event.totalBytes) memBar.peakAllocation = event.totalBytes;
      memBar.leakCount = memBar.allocCount - memBar.freeCount;
    } else if (event.type === 'asan-leak-summary') {
      // ASan leak summary
      memBar.leakCount = event.leakedAllocations;
      memBar.heapUsed = event.leakedBytes;
    } else if (event.type === 'asan-leak-location') {
      // ASan leak at specific file:line — track for inline annotations
      const key = `${event.file}:${event.line}`;
      let tracker = allocationTracker.get(key);
      if (!tracker) {
        tracker = { allocCount: 0, totalBytes: 0, leakCount: 0, pointers: new Map() };
        allocationTracker.set(key, tracker);
      }
      tracker.leakCount++;
    }
    updateMemoryBar();
  });

  // Scan on actual model change (different file opened)
  editor.onDidChangeModel(() => {
    refreshRuntimeDecorations(editor);
    scanAllAnnotations(editor);
  });

  // Rescan when content changes (debounced)
  let scanTimer: ReturnType<typeof setTimeout> | null = null;
  editor.onDidChangeModelContent(() => {
    if (scanTimer) clearTimeout(scanTimer);
    scanTimer = setTimeout(() => scanAllAnnotations(editor), 1000);
  });

  // When executedLines map changes, rescan
  store.on('executedLines', () => {
    scanAllAnnotations(editor);
  });

  // When execution finishes, do a final refresh
  store.on('isRunning', (running: boolean) => {
    if (!running) {
      finalizeLeaks();
      refreshRuntimeDecorations(editor);
      updateMemoryBar();

      const currentExec = store.get<Map<number, number>>('executedLines');
      if (currentExec && currentExec.size > 0) {
        scanAllAnnotations(editor);
        previousExecutedLines = new Map(currentExec);
      }
    }
  });
}

// ── Unified annotation scanner — produces ONE decoration per line ──
// Combines static size annotations + execution count annotations into a single pass.

let scanTimer2: ReturnType<typeof setTimeout> | null = null;
function scanAllAnnotations(editor: monaco.editor.IStandaloneCodeEditor): void {
  // Debounce — only run once after all triggers settle
  if (scanTimer2) clearTimeout(scanTimer2);
  scanTimer2 = setTimeout(() => {
    scanTimer2 = null;
    doScanAllAnnotations(editor);
  }, 50);
}

function doScanAllAnnotations(editor: monaco.editor.IStandaloneCodeEditor): void {
  const model = editor.getModel();
  if (!model) return;

  try {
    // Bump generation — old decorations with previous generation class names
    // become invisible because their CSS rules no longer exist.
    annotationGeneration++;
    const gen = annotationGeneration;

    // Nuke ALL old CSS rules. Any lingering old decorations are now invisible
    // (their class names have no matching CSS rules).
    if (annotationStyleEl) {
      annotationStyleEl.remove();
    }
    annotationStyleEl = document.createElement('style');
    annotationStyleEl.id = 'forge-annotations';
    document.head.appendChild(annotationStyleEl);
    const sheet = annotationStyleEl.sheet!;

    // Collect size annotations per line
    const sizeAnnotations = new Map<number, string>();
    collectSizeAnnotations(model, sizeAnnotations);

    // Collect exec count annotations per line
    const execAnnotations = new Map<number, string>();
    collectExecAnnotations(model, execAnnotations);

    // Merge into ONE decoration per line
    const decorations: monaco.editor.IModelDeltaDecoration[] = [];
    const allLines = new Set([...sizeAnnotations.keys(), ...execAnnotations.keys()]);

    for (const lineNum of allLines) {
      const parts: string[] = [];
      if (sizeAnnotations.has(lineNum)) parts.push(sizeAnnotations.get(lineNum)!);
      if (execAnnotations.has(lineNum)) parts.push(execAnnotations.get(lineNum)!);
      if (parts.length === 0) continue;

      const text = parts.join('  ');
      const className = `forge-ann-g${gen}-L${lineNum}`;

      try {
        sheet.insertRule(
          `.${className}::after { content: "  ${text.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"; color: #56B389; font-size: 11px; opacity: 0.7; }`,
          sheet.cssRules.length
        );
      } catch {}

      decorations.push({
        range: new monaco.Range(lineNum, 1, lineNum, model.getLineMaxColumn(lineNum)),
        options: { afterContentClassName: className },
      });
    }

    // Remove stale decorations from previous generations (cleanup, not visibility —
    // they're already invisible because their CSS rules were destroyed above)
    const staleIds = model.getAllDecorations()
      .filter(d => {
        const cls = d.options.afterContentClassName || '';
        return cls.startsWith('forge-ann-') && !cls.startsWith(`forge-ann-g${gen}-`);
      })
      .map(d => d.id);

    // deltaDecorations: remove stale, add new
    model.deltaDecorations(staleIds, decorations);
  } catch (err: any) {
    console.error('[forge] annotation scan error:', err.message);
  }
}

// ── Static size collection ──
// Parses source to find struct/class declarations, variable declarations,
// and new expressions, then estimates sizes from type information.

function collectSizeAnnotations(model: monaco.editor.ITextModel, result: Map<number, string>): void {
  const content = model.getValue();
  const lines = content.split('\n');
  const userTypes = new Map<string, number>();
  parseUserTypes(lines, userTypes);

  for (let i = 0; i < lines.length; i++) {
    const trimmed = lines[i].trim();
    const lineNum = i + 1;
    if (!trimmed || trimmed.startsWith('//') || trimmed.startsWith('#') ||
        trimmed.startsWith('/*') || trimmed.startsWith('*')) continue;

    const structDeclMatch = trimmed.match(/^(struct|class)\s+(\w+)/);
    if (structDeclMatch && trimmed.includes('{')) {
      const kind = structDeclMatch[1];
      const name = structDeclMatch[2];
      const prefixedKey = `${kind}_${name}`;
      const size = userTypes.get(prefixedKey) ?? userTypes.get(name);
      if (size !== undefined && size > 0) result.set(lineNum, `[${formatBytes(size)}]`);
      continue;
    }

    const varAnnotation = annotateVariableLine(trimmed, userTypes);
    if (varAnnotation) { result.set(lineNum, `[${varAnnotation}]`); continue; }

    const newMatch = trimmed.match(/new\s+(\w+)(?:<[^>]*>)?\s*(?:\[([^\]]+)\])?\s*(?:\(|{|;)/);
    if (newMatch) {
      const typeName = newMatch[1];
      const arraySize = newMatch[2];
      const typeSize = resolveTypeSize(typeName, userTypes);
      if (typeSize > 0) {
        if (arraySize) {
          const count = parseInt(arraySize, 10);
          if (!isNaN(count)) result.set(lineNum, `[heap ${formatBytes(typeSize)}×${count}=${formatBytes(typeSize * count)}]`);
          else result.set(lineNum, `[heap ${formatBytes(typeSize)}×${arraySize}]`);
        } else {
          result.set(lineNum, `[heap ${formatBytes(typeSize)}]`);
        }
      }
    }
  }
}

function parseUserTypes(lines: string[], userTypes: Map<string, number>): void {
  let currentType: string | null = null;
  let currentKind: string | null = null; // 'struct' or 'class'
  let braceDepth = 0;
  let memberSizes: number[] = [];
  let inType = false;

  for (const line of lines) {
    const trimmed = line.trim();

    const structMatch = trimmed.match(/^(struct|class)\s+(\w+)/);
    if (structMatch && trimmed.includes('{')) {
      currentKind = structMatch[1];
      currentType = structMatch[2];
      braceDepth = 0;
      memberSizes = [];
      inType = true;
    }

    if (inType) {
      for (const ch of trimmed) {
        if (ch === '{') braceDepth++;
        if (ch === '}') braceDepth--;
      }

      // Parse member declarations (only at depth 1 = direct members)
      if (braceDepth === 1 && !trimmed.startsWith('//') && !trimmed.startsWith('/*')) {
        const memberSize = parseMemberSize(trimmed, userTypes);
        if (memberSize > 0) {
          memberSizes.push(memberSize);
        }
      }

      if (braceDepth <= 0 && currentType && currentKind) {
        // Estimate struct size with alignment padding
        const totalSize = estimateStructSize(memberSizes);
        if (totalSize > 0) {
          // Store with kind-prefixed key to disambiguate struct vs class
          const prefixedKey = `${currentKind}_${currentType}`;
          if (!userTypes.has(prefixedKey)) {
            userTypes.set(prefixedKey, totalSize);
          }
          // Also store by plain name, but only if not already defined
          // (first definition wins — avoids later types overriding earlier ones)
          if (!userTypes.has(currentType)) {
            userTypes.set(currentType, totalSize);
          }
        }
        currentType = null;
        currentKind = null;
        inType = false;
      }
    }
  }
}

function parseMemberSize(line: string, userTypes: Map<string, number>): number {
  // Skip access specifiers, methods, constructors
  if (line.match(/^(public|private|protected)\s*:/)) return 0;
  if (line.includes('(') && !line.includes('=')) return 0; // method declaration
  if (line === '{' || line === '}' || line === '') return 0;

  // Match: type name; or type name = ...;
  // Handles both "vector<double> m;" and "vector<double>m;" (no space after >)
  const memberMatch = line.match(/^\s*([\w:<>,\s*&]+?>)\s*(\w+)\s*(?:=.*)?;/)
    || line.match(/^\s*([\w:<>,\s*&]+?)\s+(\w+)\s*(?:=.*)?;/);
  if (!memberMatch) return 0;

  const typeStr = memberMatch[1].trim();
  return resolveTypeSize(typeStr, userTypes);
}

function resolveTypeSize(typeStr: string, userTypes: Map<string, number>): number {
  // Check primitives
  if (PRIMITIVE_SIZES[typeStr] !== undefined) return PRIMITIVE_SIZES[typeStr];

  // Check pointers
  if (typeStr.endsWith('*') || typeStr.endsWith('&')) return 8;

  // Check STL types (strip template args for container size lookup)
  const stlBase = typeStr.replace(/<.*>/, '').trim();
  if (STL_SIZES[stlBase] !== undefined) {
    const size = STL_SIZES[stlBase];
    if (size > 0) return size;

    // Handle parameterized types
    if (stlBase === 'std::array') {
      const arrayMatch = typeStr.match(/std::array<(.+),\s*(\d+)>/);
      if (arrayMatch) {
        const elemSize = resolveTypeSize(arrayMatch[1].trim(), userTypes);
        const count = parseInt(arrayMatch[2], 10);
        if (elemSize > 0) return elemSize * count;
      }
    }
    if (stlBase === 'std::optional') {
      const innerMatch = typeStr.match(/std::optional<(.+)>/);
      if (innerMatch) {
        const inner = resolveTypeSize(innerMatch[1].trim(), userTypes);
        if (inner > 0) return alignTo(inner + 1, 8);
      }
    }
    if (stlBase === 'std::pair') {
      const pairMatch = typeStr.match(/std::pair<(.+),\s*(.+)>/);
      if (pairMatch) {
        const a = resolveTypeSize(pairMatch[1].trim(), userTypes);
        const b = resolveTypeSize(pairMatch[2].trim(), userTypes);
        if (a > 0 && b > 0) return alignTo(a, Math.min(b, 8)) + b;
      }
    }
    if (stlBase === 'std::atomic') {
      const innerMatch = typeStr.match(/std::atomic<(.+)>/);
      if (innerMatch) {
        return resolveTypeSize(innerMatch[1].trim(), userTypes);
      }
    }
    return 0;
  }

  // Check with std:: prefix
  if (!typeStr.startsWith('std::') && STL_SIZES['std::' + stlBase] !== undefined) {
    return STL_SIZES['std::' + stlBase];
  }

  // Check user-defined types
  if (userTypes.has(typeStr)) return userTypes.get(typeStr)!;

  // Check if it's a template of a user type
  const bareType = typeStr.replace(/<.*>/, '').trim();
  if (userTypes.has(bareType)) return userTypes.get(bareType)!;

  return 0;
}

function estimateStructSize(memberSizes: number[]): number {
  if (memberSizes.length === 0) return 0;

  // Simple alignment model: each member aligned to min(own_size, 8)
  // Struct aligned to largest member alignment
  let offset = 0;
  let maxAlign = 1;

  for (const size of memberSizes) {
    const align = Math.min(size, 8);
    maxAlign = Math.max(maxAlign, align);
    offset = alignTo(offset, align);
    offset += size;
  }

  // Pad struct to alignment
  return alignTo(offset, maxAlign);
}

function alignTo(offset: number, alignment: number): number {
  if (alignment <= 0) return offset;
  return Math.ceil(offset / alignment) * alignment;
}

function annotateVariableLine(line: string, userTypes: Map<string, number>): string | null {
  // Match local variable declarations: Type name = ...; or Type name;
  // Skip function params, return types, etc.
  const declMatch = line.match(/^\s*(const\s+)?([\w:<>,\s*&]+?>)\s*(\w+)\s*(?:[=({;])/)
    || line.match(/^\s*(const\s+)?([\w:<>,\s*&]+?)\s+(\w+)\s*(?:[=({;])/);
  if (!declMatch) return null;

  const typeStr = declMatch[2].trim();

  // Skip if it looks like a function or method call
  if (typeStr === 'return' || typeStr === 'if' || typeStr === 'for' ||
      typeStr === 'while' || typeStr === 'switch' || typeStr === 'case' ||
      typeStr === 'void' || typeStr === 'auto') return null;

  const size = resolveTypeSize(typeStr, userTypes);

  // Only annotate if the type has a known non-trivial size
  // Skip primitives under 8 bytes to avoid clutter
  if (size <= 0) return null;

  // Always show struct/class/container sizes, only show primitives if they're arrays or notable
  const isPrimitive = PRIMITIVE_SIZES[typeStr] !== undefined;
  if (isPrimitive && size <= 8) return null;

  return `${formatBytes(size)}`;
}


// ── Execution count annotations (Feature 1, 5, 6) ──

function formatExecCount(count: number): string {
  if (count >= 1_000_000) return `\u00d7${(count / 1_000_000).toFixed(1)}M`;
  if (count >= 1_000) return `\u00d7${(count / 1_000).toFixed(1)}K`;
  return `\u00d7${count}`;
}

function collectExecAnnotations(model: monaco.editor.ITextModel, result: Map<number, string>): void {
  const executedLines = store.get<Map<number, number>>('executedLines');
  if (!executedLines || executedLines.size === 0) return;

  for (const [lineNum, count] of executedLines) {
    if (lineNum < 1 || lineNum > model.getLineCount()) continue;
    if (count <= 1) continue;

    const prevCount = previousExecutedLines.get(lineNum) || 0;
    let suffix = '';
    if (prevCount > 0 && count > prevCount) suffix = '↑';
    else if (prevCount > 0 && count < prevCount) suffix = '↓';

    result.set(lineNum, `${formatExecCount(count)}${suffix}`);
  }
}

// ── Runtime tracking (unchanged) ──

function processAllocationEvent(event: AllocationEvent): void {
  const key = `${event.file}:${event.line}`;

  if (event.type === 'alloc') {
    let tracker = allocationTracker.get(key);
    if (!tracker) {
      tracker = { allocCount: 0, totalBytes: 0, leakCount: 0, pointers: new Map() };
      allocationTracker.set(key, tracker);
    }
    tracker.allocCount++;
    tracker.totalBytes += event.size;
    tracker.pointers.set(event.pointer, event.size);
    pointerIndex.set(event.pointer, key);

    memBar.allocCount++;
    memBar.heapUsed += event.size;
    if (memBar.heapUsed > memBar.peakAllocation) {
      memBar.peakAllocation = memBar.heapUsed;
    }
  } else {
    // O(1) lookup of the owning tracker via the pointer index
    const ownerKey = pointerIndex.get(event.pointer);
    if (ownerKey !== undefined) {
      const tracker = allocationTracker.get(ownerKey);
      if (tracker && tracker.pointers.has(event.pointer)) {
        const size = tracker.pointers.get(event.pointer)!;
        tracker.pointers.delete(event.pointer);
        pointerIndex.delete(event.pointer);
        memBar.freeCount++;
        memBar.heapUsed -= size;
      }
    }
  }
}

function finalizeLeaks(): void {
  memBar.leakCount = 0;
  for (const [, tracker] of allocationTracker) {
    tracker.leakCount = tracker.pointers.size;
    memBar.leakCount += tracker.leakCount;
  }
}

// Coalesce memory-bar updates. Allocation events can arrive thousands/sec
// during a training run; each store.set fans out synchronously to the memory
// bar, runtime panel, etc. Batch them so we push at most one update per frame.
let memBarDirty = false;
let memBarRaf = 0;
function updateMemoryBar(): void {
  memBarDirty = true;
  if (memBarRaf) return;
  memBarRaf = requestAnimationFrame(() => {
    memBarRaf = 0;
    if (!memBarDirty) return;
    memBarDirty = false;
    store.set('memoryBar', { ...memBar });
  });
}

export function resetMemoryTracking(): void {
  allocationTracker.clear();
  pointerIndex.clear();
  memBar = {
    heapUsed: 0, stackSize: 0, peakAllocation: 0,
    pressurePercent: 0, allocCount: 0, freeCount: 0, leakCount: 0,
  };
  store.set('memoryBar', { ...memBar });
}

function refreshRuntimeDecorations(editor: monaco.editor.IStandaloneCodeEditor): void {
  const model = editor.getModel();
  if (!model) return;

  const filePath = model.uri.fsPath;
  const newDecorations: monaco.editor.IModelDeltaDecoration[] = [];

  for (const [key, tracker] of allocationTracker) {
    const [file, lineStr] = key.split(':');
    if (file !== filePath) continue;

    const line = parseInt(lineStr, 10);
    if (line < 1 || line > model.getLineCount()) continue;

    const parts: string[] = [];
    parts.push(`${tracker.allocCount} calls`);
    parts.push(formatBytes(tracker.totalBytes));
    if (tracker.leakCount > 0) {
      parts.push(`${tracker.leakCount} leaked`);
    }

    const text = `  ← ${parts.join(', ')}`;
    const hasLeak = tracker.leakCount > 0;

    newDecorations.push({
      range: new monaco.Range(line, 10000, line, 10000),
      options: {
        after: {
          content: text,
          inlineClassName: hasLeak ? 'memory-annotation-leak' : 'memory-annotation',
        },
        isWholeLine: true,
      },
    });
  }

  decorationIds = editor.deltaDecorations(decorationIds, newDecorations);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}
