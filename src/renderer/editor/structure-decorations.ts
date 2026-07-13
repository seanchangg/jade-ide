import * as monaco from 'monaco-editor';
import type { SymbolNode } from '../panels/structure-panel';

// Background highlight for each top-level structure.
// Cycles through a palette so adjacent blocks are visually distinct —
// makes it easy to scan where classes/functions begin and end.

// Subtle, low-saturation tints — won't clash with syntax highlighting
const PALETTE = [
  'structure-tint-0',
  'structure-tint-1',
  'structure-tint-2',
  'structure-tint-3',
  'structure-tint-4',
];

let decorationIds: string[] = [];
let styleInjected = false;

function injectStyles(): void {
  if (styleInjected) return;
  styleInjected = true;
  const style = document.createElement('style');
  style.id = 'structure-tints';
  style.textContent = `
    .structure-tint-0 { background: rgba(86, 179, 137, 0.05); }
    .structure-tint-1 { background: rgba(141, 178, 255, 0.05); }
    .structure-tint-2 { background: rgba(212, 167, 106, 0.05); }
    .structure-tint-3 { background: rgba(207, 107, 107, 0.04); }
    .structure-tint-4 { background: rgba(155, 181, 207, 0.05); }
  `;
  document.head.appendChild(style);
}

export function initStructureDecorations(editor: monaco.editor.IStandaloneCodeEditor): void {
  injectStyles();

  // Expose an apply function on the editor that the structure panel can call
  (editor as any).__applyStructureTints = (symbols: SymbolNode[]) => {
    const model = editor.getModel();
    if (!model) return;

    const decorations: monaco.editor.IModelDeltaDecoration[] = [];
    let colorIdx = 0;

    // Only tint top-level symbols that span multiple lines.
    // Nested members would double-tint and make things muddy.
    for (const sym of symbols) {
      if (sym.endLine <= sym.line) continue; // skip single-line symbols
      const className = PALETTE[colorIdx % PALETTE.length];
      colorIdx++;
      decorations.push({
        range: new monaco.Range(sym.line, 1, sym.endLine, 1),
        options: {
          isWholeLine: true,
          className,
          overviewRuler: {
            color: 'rgba(255,255,255,0.05)',
            position: monaco.editor.OverviewRulerLane.Full,
          },
        },
      });
    }

    decorationIds = editor.deltaDecorations(decorationIds, decorations);
  };

  editor.onDidChangeModel(() => {
    decorationIds = editor.deltaDecorations(decorationIds, []);
  });
}

export function applyStructureTints(editor: monaco.editor.IStandaloneCodeEditor, symbols: SymbolNode[]): void {
  const fn = (editor as any).__applyStructureTints;
  if (fn) fn(symbols);
}
