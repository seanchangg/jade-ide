// ─────────────────────────────────────────────────────────────────────────────
// DebugPanel — CLion-style debug tool window docked in the bottom (terminal)
// area. Three columns while a session is live:
//   FRAMES (call stack) │ VARIABLES (locals) │ CONSOLE (lldb + program output)
// A status line in the header shows where execution is paused (file:line and
// stop reason) so it's always clear whether a breakpoint actually hit.
// While visible it takes the terminal panel's place; the terminal's previous
// visibility is restored when the session ends.
// ─────────────────────────────────────────────────────────────────────────────

import { store } from '../state';
import { setCurrentDebugLine } from '../editor/debug-decorations';
import type { LocalVariable, StackFrame } from '../../shared/types';

const forge = (window as any).forge;

export class DebugPanel {
  private container: HTMLElement;
  private statusEl: HTMLElement;
  private localsEl: HTMLElement;
  private stackEl: HTMLElement;
  private consoleEl: HTMLElement;
  private visible = false;
  private restoreTerminal = false;

  // Variable tree state: current roots + which paths are expanded (persists
  // across steps so the tree doesn't collapse on every stop).
  private locals: LocalVariable[] = [];
  private expandedPaths = new Set<string>();

  constructor(parent: HTMLElement) {
    this.container = document.createElement('div');
    this.container.className = 'debug-panel';

    // ── Header: title, status, controls ──
    const header = document.createElement('div');
    header.className = 'debug-header';

    const title = document.createElement('span');
    title.className = 'debug-title';
    title.textContent = 'DEBUG';
    header.appendChild(title);

    this.statusEl = document.createElement('span');
    this.statusEl.className = 'debug-status';
    header.appendChild(this.statusEl);

    const controls = document.createElement('div');
    controls.className = 'debug-controls';

    const btns: [string, string, () => void][] = [
      ['▶', 'Continue (F5)', () => {
        this.setStatus('Running…', 'running');
        setCurrentDebugLine(null);
        forge.debug.continue();
      }],
      ['⤵', 'Step Over (F10)', () => forge.debug.stepOver()],
      ['↓', 'Step Into (F11)', () => forge.debug.stepInto()],
      ['↑', 'Step Out (⇧F11)', () => forge.debug.stepOut()],
      ['■', 'Stop', () => forge.debug.stop()],
    ];
    for (const [icon, tooltip, action] of btns) {
      const btn = document.createElement('button');
      btn.className = 'debug-ctrl-btn';
      btn.textContent = icon;
      btn.title = tooltip;
      btn.addEventListener('click', action);
      controls.appendChild(btn);
    }
    header.appendChild(controls);
    this.container.appendChild(header);

    // ── Body: three columns ──
    const body = document.createElement('div');
    body.className = 'debug-body';

    const makeColumn = (label: string, cls: string): HTMLElement => {
      const col = document.createElement('div');
      col.className = `debug-col ${cls}`;
      const colHeader = document.createElement('div');
      colHeader.className = 'debug-col-header';
      colHeader.textContent = label;
      col.appendChild(colHeader);
      const content = document.createElement('div');
      content.className = 'debug-col-content';
      col.appendChild(content);
      body.appendChild(col);
      return content;
    };

    this.stackEl = makeColumn('FRAMES', 'debug-col-frames');
    this.localsEl = makeColumn('VARIABLES', 'debug-col-vars');
    this.consoleEl = makeColumn('CONSOLE', 'debug-col-console');

    this.container.appendChild(body);

    // Dock above the terminal panel inside the bottom area.
    parent.insertBefore(this.container, parent.firstChild);
  }

  show(): void {
    if (this.visible) return;
    this.visible = true;
    this.restoreTerminal = !!store.get<boolean>('terminalVisible');
    store.set('terminalVisible', false);
    this.container.classList.add('visible');
    this.consoleEl.textContent = '';
    this.setStatus('Launching…', 'running');
  }

  hide(): void {
    if (!this.visible) return;
    this.visible = false;
    this.container.classList.remove('visible');
    this.localsEl.innerHTML = '';
    this.stackEl.innerHTML = '';
    this.consoleEl.textContent = '';
    this.statusEl.textContent = '';
    if (this.restoreTerminal) store.set('terminalVisible', true);
  }

  /** Header status: where the run is paused / what it's doing. */
  setStatus(text: string, kind: 'running' | 'paused' | 'exited' = 'running'): void {
    this.statusEl.textContent = text;
    this.statusEl.className = `debug-status debug-status-${kind}`;
  }

  updateLocals(locals: LocalVariable[]): void {
    this.locals = locals;
    this.renderLocals();
  }

  private renderLocals(): void {
    this.localsEl.innerHTML = '';
    if (this.locals.length === 0) {
      this.localsEl.innerHTML = '<div class="debug-empty">No variables</div>';
      return;
    }
    for (const v of this.locals) this.renderVarNode(v, 0);
  }

  private renderVarNode(v: LocalVariable, depth: number): void {
    const row = document.createElement('div');
    row.className = 'debug-locals-row';
    row.style.paddingLeft = `${12 + depth * 14}px`;

    const isExpanded = !!v.path && this.expandedPaths.has(v.path);

    const caret = document.createElement('span');
    caret.className = 'debug-local-caret';
    caret.textContent = v.expandable ? (isExpanded ? '▾' : '▸') : '';
    row.appendChild(caret);

    const name = document.createElement('span');
    name.className = 'debug-local-name';
    name.textContent = v.name;
    row.appendChild(name);

    const type = document.createElement('span');
    type.className = 'debug-local-type';
    type.textContent = v.type;
    row.appendChild(type);

    const val = document.createElement('span');
    val.className = 'debug-local-value';
    val.textContent = v.value;
    val.title = v.value;
    row.appendChild(val);

    if (v.expandable) {
      row.classList.add('expandable');
      row.addEventListener('click', () => this.toggleVar(v));
    }

    this.localsEl.appendChild(row);

    if (isExpanded && v.children) {
      for (const child of v.children) this.renderVarNode(child, depth + 1);
    } else if (isExpanded && v.expandable && !v.children) {
      // Re-expanded after a step: children were parsed fresh without this
      // pointer's members — fetch them again.
      void this.fetchChildren(v);
    }
  }

  private async toggleVar(v: LocalVariable): Promise<void> {
    if (!v.path) return;
    if (this.expandedPaths.has(v.path)) {
      this.expandedPaths.delete(v.path);
      this.renderLocals();
      return;
    }
    this.expandedPaths.add(v.path);
    if (!v.children) {
      await this.fetchChildren(v);
    }
    this.renderLocals();
  }

  private async fetchChildren(v: LocalVariable): Promise<void> {
    try {
      const kids: LocalVariable[] = await forge.debug.getVarChildren(v.path);
      v.children = kids.length > 0 ? kids : undefined;
      if (!v.children) this.expandedPaths.delete(v.path!);
    } catch {
      this.expandedPaths.delete(v.path!);
    }
    this.renderLocals();
  }

  updateCallStack(frames: StackFrame[]): void {
    this.stackEl.innerHTML = '';
    if (frames.length === 0) {
      this.stackEl.innerHTML = '<div class="debug-empty">Not paused</div>';
      return;
    }
    for (const f of frames) {
      const row = document.createElement('div');
      row.className = 'debug-frame-row';
      if (f.index === 0) row.classList.add('active');

      const idx = document.createElement('span');
      idx.className = 'debug-frame-index';
      idx.textContent = `#${f.index}`;
      row.appendChild(idx);

      const func = document.createElement('span');
      func.className = 'debug-frame-func';
      func.textContent = f.functionName;
      row.appendChild(func);

      const loc = document.createElement('span');
      loc.className = 'debug-frame-loc';
      loc.textContent = `${f.file.split('/').pop()}:${f.line}`;
      loc.title = `${f.file}:${f.line}`;
      row.appendChild(loc);

      // Click to navigate
      row.addEventListener('click', () => {
        const editor = (window as any).__forgeEditor;
        if (editor && f.line > 0) {
          editor.revealLineInCenter(f.line);
          editor.setPosition({ lineNumber: f.line, column: 1 });
          editor.focus();
        }
      });

      this.stackEl.appendChild(row);
    }
  }

  appendConsoleOutput(text: string): void {
    // Strip ANSI codes for display
    const clean = text.replace(/\x1b\[[0-9;]*m/g, '');
    this.consoleEl.textContent += clean;
    this.consoleEl.scrollTop = this.consoleEl.scrollHeight;
  }
}
