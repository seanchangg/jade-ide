import * as monaco from 'monaco-editor';
import { store } from '../state';
import type { Breakpoint } from '../../shared/types';

const forge = (window as any).forge;

let editor: monaco.editor.IStandaloneCodeEditor;
let bpDecorationIds: string[] = [];
let currentLineDecorationIds: string[] = [];

// Breakpoints stored as Record<filePath, lineNumbers[]>
function getBreakpoints(): Record<string, number[]> {
  return store.get<Record<string, number[]>>('breakpoints') || {};
}

function setBreakpoints(bps: Record<string, number[]>): void {
  store.set('breakpoints', bps);
}

export function initDebugDecorations(ed: monaco.editor.IStandaloneCodeEditor): void {
  editor = ed;

  // Click glyph margin to toggle breakpoint
  editor.onMouseDown((e) => {
    if (e.target.type === monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN ||
        e.target.type === monaco.editor.MouseTargetType.GUTTER_LINE_DECORATIONS) {
      const line = e.target.position?.lineNumber;
      if (line) toggleBreakpoint(line);
    }
  });

  // Re-render breakpoint decorations when model changes
  editor.onDidChangeModel(() => {
    renderBreakpoints();
  });

  // Render on breakpoint store change
  store.on('breakpoints', () => renderBreakpoints());

  // Render on debug state change (for current line). Breakpoints re-render too
  // so the one the run is paused at gets its "active" styling.
  store.on('debugPausedLine', (line: number | null) => {
    renderCurrentLine(line);
    renderBreakpoints();
  });
}

function toggleBreakpoint(line: number): void {
  const model = editor.getModel();
  if (!model) return;
  const filePath = model.uri.fsPath;

  const bps = getBreakpoints();
  const fileBps = bps[filePath] || [];
  const idx = fileBps.indexOf(line);

  if (idx >= 0) {
    fileBps.splice(idx, 1);
    // Notify LLDB if debug session is active
    if (store.get('debugState') !== 'idle') {
      forge.debug.removeBreakpoint(filePath, line);
    }
  } else {
    fileBps.push(line);
    if (store.get('debugState') !== 'idle') {
      forge.debug.setBreakpoint(filePath, line);
    }
  }

  bps[filePath] = fileBps;
  setBreakpoints({ ...bps });
}

function renderBreakpoints(): void {
  const model = editor.getModel();
  if (!model) return;
  const filePath = model.uri.fsPath;
  const bps = getBreakpoints();
  const lines = bps[filePath] || [];

  const pausedLine = store.get<number | null>('debugPausedLine');

  const decorations: monaco.editor.IModelDeltaDecoration[] = lines.map(line => {
    const isPausedHere = line === pausedLine;
    return {
      range: new monaco.Range(line, 1, line, 1),
      options: {
        isWholeLine: true,
        className: isPausedHere ? 'debug-bp-line debug-bp-line-active' : 'debug-bp-line',
        linesDecorationsClassName: isPausedHere ? 'debug-bp-glyph debug-bp-glyph-active' : 'debug-bp-glyph',
        stickiness: monaco.editor.TrackedRangeStickiness.NeverGrowsWhenTypingAtEdges,
      },
    };
  });

  bpDecorationIds = editor.deltaDecorations(bpDecorationIds, decorations);
}

function renderCurrentLine(line: number | null): void {
  if (!line) {
    currentLineDecorationIds = editor.deltaDecorations(currentLineDecorationIds, []);
    return;
  }

  const model = editor.getModel();
  if (!model) return;

  currentLineDecorationIds = editor.deltaDecorations(currentLineDecorationIds, [{
    range: new monaco.Range(line, 1, line, model.getLineMaxColumn(line)),
    options: {
      isWholeLine: true,
      className: 'debug-current-line',
      glyphMarginClassName: 'debug-current-arrow',
      glyphMarginHoverMessage: { value: 'Execution paused here' },
    },
  }]);
}

export function setCurrentDebugLine(line: number | null): void {
  store.set('debugPausedLine', line);
  if (line && editor) {
    editor.revealLineInCenter(line);
    editor.setPosition({ lineNumber: line, column: 1 });
  }
}

export function getAllBreakpoints(): Breakpoint[] {
  const bps = getBreakpoints();
  const result: Breakpoint[] = [];
  for (const [file, lines] of Object.entries(bps)) {
    for (const line of lines) {
      result.push({ file, line });
    }
  }
  return result;
}
