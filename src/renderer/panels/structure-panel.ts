import * as monaco from 'monaco-editor';
import { store } from '../state';

// Structure panel — shows classes, functions, members as a navigable tree.
// Parses the current file's source to extract symbols.

type SymbolKind = 'class' | 'struct' | 'function' | 'method' | 'member' | 'enum' | 'namespace';

export interface SymbolNode {
  name: string;
  kind: SymbolKind;
  line: number;
  endLine: number;
  detail?: string; // return type, type annotation
  children: SymbolNode[];
}

export type { SymbolKind };

export class StructurePanel {
  private container: HTMLElement;
  private treeContainer: HTMLElement;
  private editor: monaco.editor.IStandaloneCodeEditor;
  private currentSymbols: SymbolNode[] = [];

  constructor(parent: HTMLElement, editor: monaco.editor.IStandaloneCodeEditor) {
    this.editor = editor;

    this.container = document.createElement('div');
    this.container.className = 'structure-panel';

    // Header
    const header = document.createElement('div');
    header.className = 'structure-header';
    const title = document.createElement('span');
    title.className = 'structure-title';
    title.textContent = 'STRUCTURE';
    header.appendChild(title);
    this.container.appendChild(header);

    // Tree
    this.treeContainer = document.createElement('div');
    this.treeContainer.className = 'structure-content';
    this.container.appendChild(this.treeContainer);

    parent.appendChild(this.container);

    // Refresh on file change
    store.on('activeFilePath', () => this.refresh());

    // Refresh on content change (debounced)
    let timer: ReturnType<typeof setTimeout> | null = null;
    editor.onDidChangeModelContent(() => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => this.refresh(), 800);
    });

    editor.onDidChangeModel(() => this.refresh());
  }

  refresh(): void {
    const model = this.editor.getModel();
    if (!model) {
      this.treeContainer.innerHTML = '<div class="structure-empty">No file open</div>';
      return;
    }

    const content = model.getValue();
    this.currentSymbols = parseSymbols(content);
    this.render();
    // Notify listeners (e.g. structure-decorations)
    for (const listener of this.changeListeners) listener(this.currentSymbols);
  }

  getSymbols(): SymbolNode[] {
    return this.currentSymbols;
  }

  private changeListeners: Array<(symbols: SymbolNode[]) => void> = [];
  onSymbolsChange(listener: (symbols: SymbolNode[]) => void): void {
    this.changeListeners.push(listener);
  }

  private render(): void {
    this.treeContainer.innerHTML = '';
    if (this.currentSymbols.length === 0) {
      this.treeContainer.innerHTML = '<div class="structure-empty">No symbols</div>';
      return;
    }
    this.renderNodes(this.currentSymbols, this.treeContainer, 0);
  }

  private renderNodes(nodes: SymbolNode[], parent: HTMLElement, depth: number): void {
    // Group at this level — wraps siblings so the vertical connector line spans them
    const group = document.createElement('div');
    group.className = depth === 0 ? 'structure-group structure-root' : 'structure-group';

    nodes.forEach((node, idx) => {
      const isLast = idx === nodes.length - 1;

      const nodeEl = document.createElement('div');
      nodeEl.className = 'structure-node';
      if (isLast) nodeEl.classList.add('is-last');

      // Row = elbow connector + pill
      const row = document.createElement('div');
      row.className = 'structure-row';

      // Elbow connector (only for children of a group that has depth)
      if (depth > 0) {
        const elbow = document.createElement('span');
        elbow.className = 'structure-elbow';
        row.appendChild(elbow);
      }

      // Pill (clickable) — dot + name
      const pill = document.createElement('span');
      pill.className = `structure-pill structure-pill-${node.kind}`;
      pill.title = `L${node.line}${node.detail ? '  ·  ' + node.detail : ''}`;

      const dot = document.createElement('span');
      dot.className = `structure-dot structure-dot-${node.kind}`;
      pill.appendChild(dot);

      const name = document.createElement('span');
      name.className = 'structure-name';
      name.textContent = node.name;
      pill.appendChild(name);

      pill.addEventListener('click', () => {
        this.editor.revealLineInCenter(node.line);
        this.editor.setPosition({ lineNumber: node.line, column: 1 });
        this.editor.focus();
      });

      row.appendChild(pill);
      nodeEl.appendChild(row);

      // Recurse for children (stacked beneath, indented by the group)
      if (node.children.length > 0) {
        this.renderNodes(node.children, nodeEl, depth + 1);
      }

      group.appendChild(nodeEl);
    });

    parent.appendChild(group);
  }
}

// (icons removed — using colored dots per kind instead)
function _unusedIcon(kind: SymbolKind): string {
  switch (kind) {
    case 'class': return 'C';
    case 'struct': return 'S';
    case 'function': return 'ƒ';
    case 'method': return 'm';
    case 'member': return '·';
    case 'enum': return 'E';
    case 'namespace': return 'N';
  }
}

// ── Symbol parser — extracts classes, structs, functions, members ──

function parseSymbols(source: string): SymbolNode[] {
  // Strip block comments but preserve line numbers
  const stripped = source.replace(/\/\*[\s\S]*?\*\//g, (match) => {
    return match.split('\n').map(() => '').join('\n');
  });
  const lines = stripped.split('\n');
  const symbols: SymbolNode[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i].trim();
    const lineNum = i + 1;

    // Skip single-line comments and preprocessor
    if (line.startsWith('//') || line.startsWith('#') || !line) { i++; continue; }

    // Namespace
    const nsMatch = line.match(/^namespace\s+(\w+)\s*\{/);
    if (nsMatch) {
      const end = findClosingBrace(lines, i);
      const inner = parseSymbolsRange(lines, i + 1, end);
      symbols.push({ name: nsMatch[1], kind: 'namespace', line: lineNum, endLine: end + 1, children: inner });
      i = end + 1;
      continue;
    }

    // Class or struct
    const classMatch = line.match(/^(class|struct)\s+(\w+)/);
    if (classMatch && (line.includes('{') || (i + 1 < lines.length && lines[i + 1].trim().startsWith('{')))) {
      const kind = classMatch[1] as 'class' | 'struct';
      const name = classMatch[2];
      const braceLineIdx = line.includes('{') ? i : i + 1;
      const end = findClosingBrace(lines, braceLineIdx);
      const members = parseClassMembers(lines, braceLineIdx + 1, end, name);
      symbols.push({ name, kind, line: lineNum, endLine: end + 1, children: members });
      i = end + 1;
      continue;
    }

    // Enum
    const enumMatch = line.match(/^enum\s+(?:class\s+)?(\w+)/);
    if (enumMatch) {
      const end = findClosingBrace(lines, i);
      symbols.push({ name: enumMatch[1], kind: 'enum', line: lineNum, endLine: end + 1, children: [] });
      i = end + 1;
      continue;
    }

    // Free function
    const funcMatch = line.match(/^([\w:<>*&\s]+?)\s+(\w+)\s*\([^)]*\)\s*(?:const\s*)?(?:\{|$)/);
    if (funcMatch && !line.match(/^(if|for|while|switch|do|return|class|struct|enum|namespace|typedef|using)\b/)) {
      const retType = funcMatch[1].trim();
      const name = funcMatch[2];
      const end = line.includes('{') ? findClosingBrace(lines, i) : i;
      symbols.push({ name, kind: 'function', line: lineNum, endLine: end + 1, detail: retType, children: [] });
      i = end + 1;
      continue;
    }

    i++;
  }

  return symbols;
}

function parseSymbolsRange(lines: string[], start: number, end: number): SymbolNode[] {
  const slice = lines.slice(start, end);
  return parseSymbols(slice.join('\n')).map(s => ({ ...s, line: s.line + start, endLine: s.endLine + start }));
}

function parseClassMembers(lines: string[], start: number, end: number, className: string): SymbolNode[] {
  const members: SymbolNode[] = [];
  let access = 'private';

  for (let i = start; i < end; i++) {
    const line = lines[i].trim();
    const lineNum = i + 1;

    // Access specifier
    const accMatch = line.match(/^(public|private|protected)\s*:/);
    if (accMatch) { access = accMatch[1]; continue; }

    // Skip empty lines, braces
    if (!line || line === '{' || line === '}' || line.startsWith('//')) continue;

    // Method (has parens and is not a constructor call or control flow)
    const methodMatch = line.match(/^(?:virtual\s+|static\s+|inline\s+)*([\w:<>*&\s]+?)\s+(\w+)\s*\([^)]*\)/);
    if (methodMatch && !line.match(/^(if|for|while|switch|do|return)\b/)) {
      const retType = methodMatch[1].trim();
      const name = methodMatch[2];
      // Skip constructors/destructors
      if (name === className || name === `~${className}`) {
        members.push({ name, kind: 'method', line: lineNum, endLine: lineNum, detail: 'constructor', children: [] });
      } else {
        members.push({ name, kind: 'method', line: lineNum, endLine: lineNum, detail: retType, children: [] });
      }
      // Skip function body
      if (line.includes('{')) {
        let depth = 0;
        for (let j = i; j < end; j++) {
          for (const ch of lines[j]) { if (ch === '{') depth++; if (ch === '}') depth--; }
          if (depth <= 0) { i = j; break; }
        }
      }
      continue;
    }

    // Constructor (ClassName(...))
    const ctorMatch = line.match(new RegExp(`^${className}\\s*\\(`));
    if (ctorMatch) {
      members.push({ name: className, kind: 'method', line: lineNum, endLine: lineNum, detail: 'constructor', children: [] });
      if (line.includes('{')) {
        let depth = 0;
        for (let j = i; j < end; j++) {
          for (const ch of lines[j]) { if (ch === '{') depth++; if (ch === '}') depth--; }
          if (depth <= 0) { i = j; break; }
        }
      }
      continue;
    }

    // Member variable
    const memberMatch = line.match(/^\s*([\w:<>,\s*&]+?>)\s*(\w+)\s*[;=]/)
      || line.match(/^\s*([\w:<>,\s*&]+?)\s+(\w+)\s*[;=]/);
    if (memberMatch) {
      const type = memberMatch[1].trim();
      const name = memberMatch[2];
      if (!['return', 'if', 'for', 'while', 'switch', 'case', 'break'].includes(type)) {
        members.push({ name, kind: 'member', line: lineNum, endLine: lineNum, detail: type, children: [] });
      }
    }
  }

  return members;
}

function findClosingBrace(lines: string[], startIdx: number): number {
  let depth = 0;
  for (let i = startIdx; i < lines.length; i++) {
    for (const ch of lines[i]) {
      if (ch === '{') depth++;
      if (ch === '}') depth--;
    }
    if (depth <= 0 && i > startIdx) return i;
  }
  return lines.length - 1;
}
