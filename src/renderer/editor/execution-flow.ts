import * as monaco from 'monaco-editor';
import { store } from '../state';

// Execution flow visualization using Monaco decorations (no SVG overlay).
// Glyph margin icons mark each line in the flow. CSS classes provide
// line highlighting, and call/return connections use colored markers.

let flowDecorationIds: string[] = [];
let flowStyleEl: HTMLStyleElement | null = null;
let visible = false;
let currentSegments: FlowSegment[] = [];
let hoverSvg: SVGSVGElement | null = null;
let hoverHighlightIds: string[] = [];
let hoverActive = false; // whether a hover connector is currently drawn
let flowDebounceTimer: ReturnType<typeof setTimeout> | null = null;

// Colors for different flow types
const FLOW_COLORS = {
  sequential: '#56B389',  // accent green — normal flow
  call: '#8DB2FF',        // periwinkle — function call
  returnFlow: '#D4A76A',  // amber — returning from call
  loop: '#8DB2FF',        // periwinkle — loop back
  branch: '#D4A76A',      // amber — branch
  error: '#CF6B6B',       // red — error
};

interface FlowSegment {
  fromLine: number;
  toLine: number;
  type: 'sequential' | 'call' | 'return' | 'branch' | 'loop-back';
  label?: string;
}

export function initExecutionFlow(
  editor: monaco.editor.IStandaloneCodeEditor,
  _editorContainer: HTMLElement
): void {
  // Create dynamic stylesheet for glyph margin icons
  flowStyleEl = document.createElement('style');
  flowStyleEl.id = 'forge-flow-styles';
  document.head.appendChild(flowStyleEl);

  // Base CSS classes for flow decorations
  flowStyleEl.textContent = `
    .flow-glyph-seq::after,
    .flow-glyph-call::after,
    .flow-glyph-return::after,
    .flow-glyph-loop::after,
    .flow-glyph-branch::after,
    .flow-glyph-error::after {
      display: flex;
      align-items: center;
      justify-content: center;
      width: 100%;
      height: 100%;
      font-size: 12px;
      line-height: 1;
    }
    .flow-glyph-seq::after {
      content: '●';
      color: ${FLOW_COLORS.sequential};
    }
    .flow-glyph-call::after {
      content: '→';
      color: ${FLOW_COLORS.call};
      font-weight: bold;
    }
    .flow-glyph-return::after {
      content: '↩';
      color: ${FLOW_COLORS.returnFlow};
    }
    .flow-glyph-loop::after {
      content: '↺';
      color: ${FLOW_COLORS.loop};
    }
    .flow-glyph-branch::after {
      content: '⑂';
      color: ${FLOW_COLORS.branch};
      font-size: 14px;
    }
    .flow-glyph-error::after {
      content: '⊘';
      color: ${FLOW_COLORS.error};
      font-size: 14px;
      font-weight: bold;
    }
    .flow-line-seq {
      background: rgba(86, 179, 137, 0.06) !important;
      border-left: 2px solid ${FLOW_COLORS.sequential}40 !important;
    }
    .flow-line-call {
      background: rgba(141, 178, 255, 0.08) !important;
      border-left: 2px solid ${FLOW_COLORS.call}60 !important;
    }
    .flow-line-return {
      background: rgba(212, 167, 106, 0.06) !important;
      border-left: 2px solid ${FLOW_COLORS.returnFlow}40 !important;
    }
    .flow-line-loop {
      background: rgba(141, 178, 255, 0.06) !important;
      border-left: 2px solid ${FLOW_COLORS.loop}40 !important;
    }
    .flow-line-branch {
      background: rgba(212, 167, 106, 0.06) !important;
      border-left: 2px solid ${FLOW_COLORS.branch}40 !important;
    }
    .flow-line-error {
      background: rgba(207, 107, 107, 0.12) !important;
      border-left: 2px solid ${FLOW_COLORS.error}80 !important;
    }
    .flow-line-executed {
      background: rgba(86, 179, 137, 0.08) !important;
    }
    .flow-glyph-dimmed::after {
      opacity: 0.25 !important;
    }
    .flow-hover-target {
      background: rgba(141, 178, 255, 0.15) !important;
      outline: 1px solid rgba(141, 178, 255, 0.3);
      outline-offset: -1px;
    }
    .line-executed {
      background: rgba(86, 179, 137, 0.04) !important;
    }
  `;

  // Toggle visibility
  store.on('executionFlowVisible', (show: boolean) => {
    visible = show;
    if (show) {
      applyFlowDecorations(editor);
    } else {
      clearDecorations(editor);
    }
  });

  // Re-render when model changes
  editor.onDidChangeModel(() => {
    if (visible) applyFlowDecorations(editor);
  });

  editor.onDidChangeModelContent(() => {
    if (!visible) return;
    // Debounce — full-file flow analysis is expensive, and without clearing the
    // previous timer every keystroke would queue another redundant analysis.
    if (flowDebounceTimer) clearTimeout(flowDebounceTimer);
    flowDebounceTimer = setTimeout(() => {
      flowDebounceTimer = null;
      applyFlowDecorations(editor);
    }, 300);
  });

  // Re-render when executed lines update (post-run)
  store.on('executedLines', () => {
    if (visible) applyFlowDecorations(editor);
  });

  // ── Hover connector lines ──
  // When the mouse enters a glyph margin on a flow line, draw a connector
  // to the related line (call target, return source, loop start).

  // Create a persistent SVG overlay for hover connectors
  // Attach to the editor's overflow guard (the visible viewport area)
  hoverSvg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  hoverSvg.style.cssText = 'position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none;z-index:10;overflow:visible;';
  const editorDom = editor.getDomNode();
  const overflowGuard = editorDom?.querySelector('.overflow-guard') as HTMLElement;
  if (overflowGuard) {
    overflowGuard.style.position = 'relative';
    overflowGuard.appendChild(hoverSvg);
  } else if (editorDom) {
    editorDom.style.position = 'relative';
    editorDom.appendChild(hoverSvg);
  }

  editor.onMouseMove((e) => {
    if (!visible || !hoverSvg) return;

    // Only react to glyph margin hover. When not over the glyph margin, avoid
    // touching the DOM/decorations on every mouse move — only clear if we
    // actually have a connector drawn (prevents per-move layout thrash).
    if (e.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN) {
      if (hoverActive) {
        hoverSvg.innerHTML = '';
        hoverHighlightIds = editor.deltaDecorations(hoverHighlightIds, []);
        hoverActive = false;
      }
      return;
    }

    // Clear previous hover connector before drawing the new one
    hoverSvg.innerHTML = '';
    hoverHighlightIds = editor.deltaDecorations(hoverHighlightIds, []);

    const line = e.target.position?.lineNumber;
    if (!line) { hoverActive = false; return; }

    // Find only INTERESTING segments (call, return, loop-back, branch) — skip sequential
    const connections: Array<{ from: number; to: number; color: string }> = [];
    for (const seg of currentSegments) {
      if (seg.type === 'sequential') continue; // skip — these are just neighbors
      if (seg.fromLine === line || seg.toLine === line) {
        let color = FLOW_COLORS.sequential;
        if (seg.type === 'call') color = FLOW_COLORS.call;
        else if (seg.type === 'return') color = FLOW_COLORS.returnFlow;
        else if (seg.type === 'loop-back') color = FLOW_COLORS.loop;
        else if (seg.type === 'branch') color = FLOW_COLORS.branch;

        connections.push({ from: seg.fromLine, to: seg.toLine, color });
      }
    }

    if (connections.length === 0) { hoverActive = false; return; }

    const model = editor.getModel();
    if (!model) { hoverActive = false; return; }

    // Draw connector lines and highlight target lines
    const layoutInfo = editor.getLayoutInfo();
    const glyphCenter = layoutInfo.glyphMarginLeft + layoutInfo.glyphMarginWidth / 2;
    const lineHeight = editor.getOption(monaco.editor.EditorOption.lineHeight);
    const padding = editor.getOption(monaco.editor.EditorOption.padding);
    const padTop = (padding as any)?.top || 0;

    const highlightDecos: monaco.editor.IModelDeltaDecoration[] = [];

    for (const conn of connections) {
      const y1 = editor.getTopForLineNumber(conn.from) - editor.getScrollTop() + lineHeight / 2 + padTop;
      const y2 = editor.getTopForLineNumber(conn.to) - editor.getScrollTop() + lineHeight / 2 + padTop;

      // Draw the connector line
      const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      const midY = (y1 + y2) / 2;
      const curveX = glyphCenter + 20;
      path.setAttribute('d', `M ${glyphCenter} ${y1} C ${curveX} ${midY}, ${curveX} ${midY}, ${glyphCenter} ${y2}`);
      path.setAttribute('stroke', conn.color);
      path.setAttribute('stroke-width', '2.5');
      path.setAttribute('stroke-linecap', 'round');
      path.setAttribute('fill', 'none');
      path.setAttribute('opacity', '0.8');
      hoverSvg.appendChild(path);

      // Small circles at both ends
      for (const y of [y1, y2]) {
        const dot = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
        dot.setAttribute('cx', String(glyphCenter));
        dot.setAttribute('cy', String(y));
        dot.setAttribute('r', '4');
        dot.setAttribute('fill', conn.color);
        dot.setAttribute('opacity', '0.9');
        hoverSvg.appendChild(dot);
      }

      // Highlight the OTHER line (the one that isn't being hovered)
      const targetLine = conn.from === line ? conn.to : conn.from;
      if (targetLine >= 1 && targetLine <= model.getLineCount()) {
        highlightDecos.push({
          range: new monaco.Range(targetLine, 1, targetLine, 1),
          options: {
            isWholeLine: true,
            className: 'flow-hover-target',
          },
        });
      }
    }

    hoverHighlightIds = editor.deltaDecorations(hoverHighlightIds, highlightDecos);
    hoverActive = true;
  });

  // Clear on mouse leave
  editor.onMouseLeave(() => {
    if (hoverSvg) hoverSvg.innerHTML = '';
    hoverHighlightIds = editor.deltaDecorations(hoverHighlightIds, []);
    hoverActive = false;
  });

  // Feature 2: Cmd+click on flow dots to jump to function
  editor.onMouseDown((e) => {
    if (!visible) return;
    // Check for Cmd (Meta) click on glyph margin
    if (!e.event.metaKey && !e.event.ctrlKey) return;
    if (e.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN) return;

    const line = e.target.position?.lineNumber;
    if (!line) return;

    // Find call or return segments for this line
    for (const seg of currentSegments) {
      if (seg.type === 'call' && seg.fromLine === line) {
        // Jump to call target
        editor.revealLineInCenter(seg.toLine);
        editor.setPosition({ lineNumber: seg.toLine, column: 1 });
        e.event.preventDefault();
        return;
      }
      if (seg.type === 'return' && seg.toLine === line) {
        // Jump back to where the return came from
        editor.revealLineInCenter(seg.fromLine);
        editor.setPosition({ lineNumber: seg.fromLine, column: 1 });
        e.event.preventDefault();
        return;
      }
      if (seg.type === 'call' && seg.toLine === line) {
        // On a function entry, jump back to the call site
        editor.revealLineInCenter(seg.fromLine);
        editor.setPosition({ lineNumber: seg.fromLine, column: 1 });
        e.event.preventDefault();
        return;
      }
      if (seg.type === 'return' && seg.fromLine === line) {
        // On a return statement, jump to where it returns to
        editor.revealLineInCenter(seg.toLine);
        editor.setPosition({ lineNumber: seg.toLine, column: 1 });
        e.event.preventDefault();
        return;
      }
    }
  });
}

function applyFlowDecorations(editor: monaco.editor.IStandaloneCodeEditor): void {
  const model = editor.getModel();
  if (!model) return;

  const content = model.getValue();
  const segments = analyzeFlowFromSource(content);
  currentSegments = segments; // store for hover connector lookups
  const executedLines = store.get<Map<number, number>>('executedLines') || new Map<number, number>();
  const hasRunData = executedLines.size > 0;
  const errorLine = store.get<number>('errorLine');

  const decorations: monaco.editor.IModelDeltaDecoration[] = [];

  // Track which lines are part of the flow and their type
  const lineTypes = new Map<number, string>();

  for (const seg of segments) {
    // Only mark the actual from/to lines — NOT everything in between
    const type = seg.type === 'return' ? 'return' : seg.type;

    // Prefer more specific types over sequential
    if (!lineTypes.has(seg.toLine) || lineTypes.get(seg.toLine) === 'sequential') {
      lineTypes.set(seg.toLine, type);
    }
    if (!lineTypes.has(seg.fromLine)) {
      lineTypes.set(seg.fromLine, seg.type === 'call' ? 'call' : 'sequential');
    }
  }

  // Create decorations for each line in the flow
  for (const [line, type] of lineTypes) {
    if (line < 1 || line > model.getLineCount()) continue;

    let glyphClass = 'flow-glyph-seq';
    let lineClass = 'flow-line-seq';

    switch (type) {
      case 'call':
        glyphClass = 'flow-glyph-call';
        lineClass = 'flow-line-call';
        break;
      case 'return':
        glyphClass = 'flow-glyph-return';
        lineClass = 'flow-line-return';
        break;
      case 'loop-back':
        glyphClass = 'flow-glyph-loop';
        lineClass = 'flow-line-loop';
        break;
      case 'branch':
        glyphClass = 'flow-glyph-branch';
        lineClass = 'flow-line-branch';
        break;
    }

    // If we have run data, dim unexecuted lines and highlight executed ones
    let lineHighlight = '';
    if (hasRunData) {
      if (executedLines.has(line)) {
        lineHighlight = 'flow-line-executed';
      } else {
        glyphClass += ' flow-glyph-dimmed';
      }
    }

    decorations.push({
      range: new monaco.Range(line, 1, line, 1),
      options: {
        glyphMarginClassName: glyphClass,
        glyphMarginHoverMessage: { value: getHoverMessage(type, line, segments) },
        ...(lineHighlight ? { isWholeLine: true, className: lineHighlight } : {}),
      },
    });
  }

  // Mark error line
  if (errorLine && errorLine >= 1 && errorLine <= model.getLineCount()) {
    decorations.push({
      range: new monaco.Range(errorLine, 1, errorLine, 1),
      options: {
        isWholeLine: true,
        className: 'flow-line-error',
        glyphMarginClassName: 'flow-glyph-error',
      },
    });
  }

  // Post-run: highlight executed lines
  if (hasRunData) {
    for (const [line] of executedLines) {
      if (line >= 1 && line <= model.getLineCount() && !lineTypes.has(line)) {
        decorations.push({
          range: new monaco.Range(line, 1, line, 1),
          options: {
            isWholeLine: true,
            className: 'line-executed',
          },
        });
      }
    }
  }

  flowDecorationIds = editor.deltaDecorations(flowDecorationIds, decorations);
}

function getHoverMessage(type: string, line: number, segments: FlowSegment[]): string {
  const parts: string[] = [];
  const seen = new Set<string>(); // dedup

  for (const seg of segments) {
    let msg = '';

    // This line calls a function
    if (seg.type === 'call' && seg.fromLine === line) {
      msg = `→ calls \`${seg.label || 'function'}\` (line ${seg.toLine})`;
    }
    // This line IS a function that gets called from somewhere
    if (seg.type === 'call' && seg.toLine === line) {
      msg = `← called from line ${seg.fromLine}`;
    }
    // This line is where a function returns to
    if (seg.type === 'return' && seg.toLine === line) {
      msg = `↩ return from line ${seg.fromLine}`;
    }
    // This line returns to a caller
    if (seg.type === 'return' && seg.fromLine === line) {
      msg = `↩ returns to line ${seg.toLine}`;
    }
    // Loop back
    if (seg.type === 'loop-back' && seg.fromLine === line) {
      msg = `↺ loop back to line ${seg.toLine}`;
    }
    if (seg.type === 'loop-back' && seg.toLine === line) {
      msg = `↺ loop from line ${seg.fromLine}`;
    }
    // Branch
    if (seg.type === 'branch' && seg.toLine === line) {
      msg = `⑂ branch`;
    }

    if (msg && !seen.has(msg)) {
      seen.add(msg);
      parts.push(msg);
    }
  }

  if (parts.length === 0) return `Flow: line ${line}`;
  return parts.map(p => `**${p}**`).join('\n\n');
}

function clearDecorations(editor: monaco.editor.IStandaloneCodeEditor): void {
  flowDecorationIds = editor.deltaDecorations(flowDecorationIds, []);
}

// ── Static analysis: trace ALL function bodies, resolve cross-function calls ──

function analyzeFlowFromSource(source: string): FlowSegment[] {
  const lines = source.split('\n');
  const segments: FlowSegment[] = [];

  // Pass 1: Find all function/method definitions with their line ranges
  // Store ALL definitions per name to handle overloaded/same-name methods in different classes
  interface FuncDef { startLine: number; endLine: number; bodyStart: number; className?: string; qualifiedName: string }
  const funcDefsAll = new Map<string, FuncDef[]>(); // shortName → all defs
  const allFuncRanges: Array<{ name: string; startIdx: number; endIdx: number; startLine: number }> = [];

  // Also track which class each method belongs to
  let currentClassName: string | null = null;
  let classBraceDepth = 0;
  let inClassBody = false;

  // Pre-scan for class definitions to know which class context we're in
  const classRanges: Array<{ name: string; startLine: number; endLine: number }> = [];
  let tmpClass: string | null = null;
  let tmpDepth = 0;
  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    const cm = t.match(/^(?:struct|class)\s+(\w+)/);
    if (cm && t.includes('{') && !tmpClass) {
      tmpClass = cm[1]; tmpDepth = 0;
    }
    if (tmpClass) {
      for (const ch of t) { if (ch === '{') tmpDepth++; if (ch === '}') tmpDepth--; }
      if (tmpDepth <= 0) {
        classRanges.push({ name: tmpClass, startLine: i - 10, endLine: i + 1 }); // approximate
        tmpClass = null;
      }
    }
  }

  function getClassForLine(line: number): string | null {
    for (const cr of classRanges) {
      if (line >= cr.startLine && line <= cr.endLine) return cr.name;
    }
    return null;
  }

  let curFunc: string | null = null;
  let curFuncDepth = 0;
  let curFuncStartIdx = 0;
  let curFuncStartLine = 0;

  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    const m = t.match(/^(?:[\w:<>*&\s]+?)\s+(\w+(?:::\w+)?)\s*\([^)]*\)\s*(?:const\s*)?(?:override\s*)?(?:noexcept\s*)?\{/);
    if (m && !t.match(/^(if|for|while|switch|do)\b/) && !curFunc) {
      curFunc = m[1];
      curFuncDepth = 0;
      curFuncStartIdx = i;
      curFuncStartLine = i + 1;
    }
    if (curFunc) {
      for (const ch of t) {
        if (ch === '{') curFuncDepth++;
        if (ch === '}') curFuncDepth--;
      }
      if (curFuncDepth <= 0) {
        const shortName = curFunc.includes('::') ? curFunc.split('::').pop()! : curFunc;
        const className = getClassForLine(curFuncStartLine);
        const def: FuncDef = {
          startLine: curFuncStartLine, endLine: i + 1, bodyStart: curFuncStartIdx,
          className: className || undefined, qualifiedName: curFunc,
        };
        if (!funcDefsAll.has(shortName)) funcDefsAll.set(shortName, []);
        funcDefsAll.get(shortName)!.push(def);
        allFuncRanges.push({ name: shortName, startIdx: curFuncStartIdx, endIdx: i, startLine: curFuncStartLine });
        curFunc = null;
      }
    }
  }

  // Helper: resolve a variable's type from context (look for declarations above the call)
  function resolveVarType(varName: string, contextLines: string[], upToIdx: number): string | null {
    for (let j = upToIdx; j >= 0; j--) {
      const t = contextLines[j].trim();
      // Match: Type varName(...) or Type varName = ...
      const dm = t.match(new RegExp(`(\\w+)\\s+${varName}\\s*[=(]`));
      if (dm) return dm[1];
      // Match: Type* varName or Type& varName
      const pm = t.match(new RegExp(`(\\w+)[*&]\\s+${varName}\\b`));
      if (pm) return pm[1];
    }
    return null;
  }

  // Helper: pick the best funcDef for a call, considering the object's type
  function resolveFuncDef(
    calledName: string, objectType: string | null, callLine: number, ownStartLine: number
  ): FuncDef | null {
    const defs = funcDefsAll.get(calledName);
    if (!defs || defs.length === 0) return null;
    if (defs.length === 1) {
      return defs[0].startLine !== ownStartLine ? defs[0] : null;
    }

    // Try to match by class name
    if (objectType) {
      const match = defs.find(d => d.className === objectType && d.startLine !== ownStartLine);
      if (match) return match;
    }

    // Fall back to closest definition by line proximity
    let best: FuncDef | null = null;
    let bestDist = Infinity;
    for (const d of defs) {
      if (d.startLine === ownStartLine) continue;
      const dist = Math.abs(d.startLine - callLine);
      if (dist < bestDist) { bestDist = dist; best = d; }
    }
    return best;
  }

  // Pass 2: Trace EVERY function body
  for (const func of allFuncRanges) {
    traceBody(lines, func.startIdx, func.endIdx, resolveFuncDef, func.startLine, segments);
  }

  return segments;
}

function traceBody(
  lines: string[],
  startIdx: number,
  endIdx: number,
  resolveFuncDef: (name: string, objType: string | null, callLine: number, ownStart: number) => { startLine: number; endLine: number } | null,
  ownStartLine: number,
  segments: FlowSegment[]
): void {
  let braceDepth = 0;
  let entered = false;
  let lastLine = -1;
  const loopStack: Array<{ line: number; depth: number }> = [];

  for (let i = startIdx; i <= endIdx; i++) {
    const line = lines[i].trim();
    const lineNum = i + 1;

    const prevDepth = braceDepth;
    for (const ch of line) {
      if (ch === '{') { braceDepth++; entered = true; }
      if (ch === '}') braceDepth--;
    }

    if (entered && braceDepth <= 0) break;
    if (!entered) continue;
    if (!line || line === '{' || line === '}' || line.startsWith('//')) continue;

    // Loop-back
    if (braceDepth < prevDepth && loopStack.length > 0) {
      const top = loopStack[loopStack.length - 1];
      if (braceDepth <= top.depth) {
        loopStack.pop();
        segments.push({ fromLine: lineNum, toLine: top.line, type: 'loop-back' });
        lastLine = lineNum;
      }
      continue;
    }

    // Loops
    if (line.match(/^(for|while|do)\b/)) {
      if (lastLine > 0) segments.push({ fromLine: lastLine, toLine: lineNum, type: 'sequential' });
      loopStack.push({ line: lineNum, depth: prevDepth });
      lastLine = lineNum;
      continue;
    }

    // Branches
    if (line.match(/^(if|else|switch)\b/)) {
      if (lastLine > 0) segments.push({ fromLine: lastLine, toLine: lineNum, type: 'branch' });
      lastLine = lineNum;
      continue;
    }

    // Function/method calls — extract object name for type resolution
    const callMatch = line.match(/(?:(\w+)\s*[\.\->]+\s*)?(\w+)\s*\(/);
    if (callMatch && !line.match(/^(if|for|while|switch|return|case|class|struct|int|void|double|float|auto|bool|char)\b/)) {
      if (lastLine > 0 && lastLine !== lineNum) {
        segments.push({ fromLine: lastLine, toLine: lineNum, type: 'sequential' });
      }

      const objectName = callMatch[1] || null; // e.g. "op1" from "op1.print_info()"
      const calledName = callMatch[2];

      // Try to resolve the object's type from variable declarations above this line
      let objectType: string | null = null;
      if (objectName) {
        for (let j = i - 1; j >= startIdx; j--) {
          const prev = lines[j].trim();
          const typeMatch = prev.match(new RegExp(`(\\w+)\\s+${objectName}\\s*[=(]`));
          if (typeMatch) { objectType = typeMatch[1]; break; }
          const ptrMatch = prev.match(new RegExp(`(\\w+)[*&<].*\\s+${objectName}\\b`));
          if (ptrMatch) { objectType = ptrMatch[1]; break; }
        }
      }

      const def = resolveFuncDef(calledName, objectType, lineNum, ownStartLine);
      if (def) {
        const label = objectType ? `${objectType}::${calledName}` : calledName;
        segments.push({ fromLine: lineNum, toLine: def.startLine, type: 'call', label });
        segments.push({ fromLine: def.endLine, toLine: lineNum, type: 'return' });
      }

      lastLine = lineNum;
      continue;
    }

    // Regular statement
    if (lastLine > 0 && lastLine !== lineNum) {
      segments.push({ fromLine: lastLine, toLine: lineNum, type: 'sequential' });
    }
    lastLine = lineNum;
  }
}
