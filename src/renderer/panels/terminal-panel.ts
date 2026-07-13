import { Terminal } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { WebLinksAddon } from 'xterm-addon-web-links';
import { store } from '../state';

const forge = (window as any).forge;

// Chrome (background/cursor/ANSI black) is shadcn zinc to match the editor;
// the 16 ANSI colors stay a conventional palette so program output (ls, git,
// compiler diagnostics) renders as expected. xterm paints to canvas, so the
// theme must be swapped programmatically on light/dark toggle — CSS variables
// don't reach it.
const TERMINAL_THEME_DARK = {
  background: '#1E1F22',
  foreground: '#DFE1E5',
  cursor: '#56B389',
  cursorAccent: '#1E1F22',
  selectionBackground: '#56B38930',
  selectionForeground: '#DFE1E5',
  black: '#2B2D30', red: '#CF6B6B', green: '#56B389', yellow: '#D4A76A',
  blue: '#8DB2FF', magenta: '#9BB5CF', cyan: '#7A9484', white: '#DFE1E5',
  brightBlack: '#6F737A', brightRed: '#CF6B6B', brightGreen: '#56B389',
  brightYellow: '#D4A76A', brightBlue: '#8DB2FF', brightMagenta: '#9BB5CF',
  brightCyan: '#7A9484', brightWhite: '#DFE1E5',
};

// Same hues, darkened for contrast on a light background.
const TERMINAL_THEME_LIGHT = {
  background: '#FFFFFF',
  foreground: '#1F2328',
  cursor: '#2E7D5B',
  cursorAccent: '#FFFFFF',
  selectionBackground: '#2E7D5B26',
  selectionForeground: '#1F2328',
  black: '#1F2328', red: '#B3403F', green: '#2E7D5B', yellow: '#9A6700',
  blue: '#2F5FD0', magenta: '#6E5494', cyan: '#3E6D5D', white: '#E8EAED',
  brightBlack: '#6F737A', brightRed: '#B3403F', brightGreen: '#2E7D5B',
  brightYellow: '#9A6700', brightBlue: '#2F5FD0', brightMagenta: '#6E5494',
  brightCyan: '#3E6D5D', brightWhite: '#F5F6F8',
};

function currentTerminalTheme(): typeof TERMINAL_THEME_DARK {
  return document.body.classList.contains('light-mode')
    ? TERMINAL_THEME_LIGHT
    : TERMINAL_THEME_DARK;
}

interface TermInstance {
  id: number;
  name: string;
  ptyId: number;
  terminal: Terminal;
  fitAddon: FitAddon;
  el: HTMLElement;
}

export class TerminalPanel {
  private container: HTMLElement;
  private headerEl: HTMLElement;
  private listEl: HTMLElement;
  private termAreaEl: HTMLElement;
  private resizeHandle: HTMLElement;

  private instances: TermInstance[] = [];
  private activeId = -1;
  private nextId = 1;
  private listVisible = false;

  constructor(parent: HTMLElement) {
    this.container = document.createElement('div');
    this.container.className = 'terminal-panel';

    // Resize handle
    this.resizeHandle = document.createElement('div');
    this.resizeHandle.className = 'terminal-resize-handle';
    this.container.appendChild(this.resizeHandle);

    // Header
    this.headerEl = document.createElement('div');
    this.headerEl.className = 'terminal-header';

    const title = document.createElement('span');
    title.className = 'terminal-title';
    title.textContent = 'TERMINAL';
    this.headerEl.appendChild(title);

    const controls = document.createElement('div');
    controls.className = 'terminal-controls';

    // Toggle list button
    const listBtn = document.createElement('span');
    listBtn.className = 'terminal-ctrl-btn';
    listBtn.textContent = '≡';
    listBtn.title = 'Terminal list';
    listBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      this.toggleList();
    });
    controls.appendChild(listBtn);

    // New terminal button
    const newBtn = document.createElement('span');
    newBtn.className = 'terminal-ctrl-btn';
    newBtn.textContent = '+';
    newBtn.title = 'New terminal';
    newBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      this.createInstance();
    });
    controls.appendChild(newBtn);

    // Minimize button
    const minBtn = document.createElement('span');
    minBtn.className = 'terminal-ctrl-btn';
    minBtn.textContent = '−';
    minBtn.title = 'Minimize terminal (⌘`)';
    minBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      store.set('terminalVisible', false);
    });
    controls.appendChild(minBtn);

    this.headerEl.appendChild(controls);

    // Click header to expand when minimized
    this.headerEl.addEventListener('click', () => {
      if (!store.get<boolean>('terminalVisible')) {
        store.set('terminalVisible', true);
      }
    });
    this.headerEl.style.cursor = 'pointer';

    this.container.appendChild(this.headerEl);

    // Terminal list (hidden by default)
    this.listEl = document.createElement('div');
    this.listEl.className = 'terminal-list';
    this.container.appendChild(this.listEl);

    // Terminal display area
    this.termAreaEl = document.createElement('div');
    this.termAreaEl.className = 'terminal-content';
    this.container.appendChild(this.termAreaEl);

    parent.appendChild(this.container);

    // Visibility toggle
    store.on('terminalVisible', (visible: boolean) => {
      if (visible) {
        this.container.classList.add('visible');
        const h = store.get<number>('terminalHeight') || 200;
        this.container.style.height = `${h}px`;
        if (this.instances.length === 0) {
          this.createInstance();
        } else {
          this.getActive()?.fitAddon.fit();
        }
      } else {
        this.container.classList.remove('visible');
        this.container.style.height = '26px';
      }
    });

    this.initResize();

    store.on('terminalHeight', (height: number) => {
      this.container.style.height = `${height}px`;
      this.getActive()?.fitAddon.fit();
    });

    // Retheme live terminals on light/dark toggle — xterm renders to canvas,
    // so the CSS variable flip alone doesn't reach it.
    store.on('themeMode', () => {
      const theme = currentTerminalTheme();
      for (const inst of this.instances) {
        inst.terminal.options.theme = theme;
      }
    });
  }

  private async createInstance(): Promise<TermInstance> {
    const id = this.nextId++;
    const name = `zsh ${id}`;

    const el = document.createElement('div');
    el.className = 'terminal-instance';
    el.style.display = 'none';
    this.termAreaEl.appendChild(el);

    const terminal = new Terminal({
      theme: currentTerminalTheme(),
      fontFamily: "'JetBrains Mono', 'SF Mono', Menlo, 'Courier New', monospace",
      fontSize: 13,
      lineHeight: 1.4,
      cursorBlink: true,
      cursorStyle: 'bar',
      scrollback: 10000,
      allowProposedApi: true,
    });

    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.loadAddon(new WebLinksAddon());
    terminal.open(el);

    const workspaceRoot = store.get<string>('workspaceRoot') || '';
    const ptyId = await forge.terminal.create(workspaceRoot);

    if (ptyId >= 0) {
      terminal.onData((data) => forge.terminal.write(ptyId, data));

      forge.terminal.onData((tid: number, data: string) => {
        if (tid === ptyId) terminal.write(data);
      });

      forge.terminal.onExit((tid: number, code: number) => {
        if (tid === ptyId) {
          terminal.write(`\r\n\x1b[90m[exited ${code}]\x1b[0m\r\n`);
        }
      });

      terminal.onResize(({ cols, rows }) => {
        forge.terminal.resize(ptyId, cols, rows);
      });
    }

    const observer = new ResizeObserver(() => fitAddon.fit());
    observer.observe(el);

    const inst: TermInstance = { id, name, ptyId, terminal, fitAddon, el };
    this.instances.push(inst);
    this.switchTo(id);
    this.renderList();

    return inst;
  }

  private switchTo(id: number): void {
    this.activeId = id;
    for (const inst of this.instances) {
      inst.el.style.display = inst.id === id ? 'block' : 'none';
    }
    const active = this.getActive();
    if (active) {
      requestAnimationFrame(() => {
        active.fitAddon.fit();
        active.terminal.focus();
      });
    }
    this.renderList();
  }

  private deleteInstance(id: number): void {
    const idx = this.instances.findIndex(i => i.id === id);
    if (idx < 0) return;

    const inst = this.instances[idx];
    if (inst.ptyId >= 0) forge.terminal.destroy(inst.ptyId);
    inst.terminal.dispose();
    inst.el.remove();
    this.instances.splice(idx, 1);

    if (this.activeId === id) {
      if (this.instances.length > 0) {
        this.switchTo(this.instances[Math.max(0, idx - 1)].id);
      } else {
        this.activeId = -1;
      }
    }
    this.renderList();
  }

  private duplicateInstance(id: number): void {
    this.createInstance();
  }

  private toggleList(): void {
    this.listVisible = !this.listVisible;
    this.listEl.style.display = this.listVisible ? 'block' : 'none';
    this.renderList();
  }

  private renderList(): void {
    if (!this.listVisible) return;
    this.listEl.innerHTML = '';

    for (const inst of this.instances) {
      const row = document.createElement('div');
      row.className = `terminal-list-row ${inst.id === this.activeId ? 'active' : ''}`;

      const icon = document.createElement('span');
      icon.className = 'terminal-list-icon';
      icon.textContent = '>_';
      row.appendChild(icon);

      const name = document.createElement('span');
      name.className = 'terminal-list-name';
      name.textContent = inst.name;
      row.appendChild(name);

      const actions = document.createElement('span');
      actions.className = 'terminal-list-actions';

      const dupBtn = document.createElement('span');
      dupBtn.className = 'terminal-list-action';
      dupBtn.textContent = '⧉';
      dupBtn.title = 'Duplicate';
      dupBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        this.duplicateInstance(inst.id);
      });
      actions.appendChild(dupBtn);

      const delBtn = document.createElement('span');
      delBtn.className = 'terminal-list-action';
      delBtn.textContent = '🗑';
      delBtn.title = 'Delete';
      delBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        this.deleteInstance(inst.id);
      });
      actions.appendChild(delBtn);

      row.appendChild(actions);

      row.addEventListener('click', () => this.switchTo(inst.id));
      this.listEl.appendChild(row);
    }
  }

  private getActive(): TermInstance | undefined {
    return this.instances.find(i => i.id === this.activeId);
  }

  private initResize(): void {
    let startY = 0, startHeight = 0;
    this.resizeHandle.addEventListener('mousedown', (e) => {
      startY = e.clientY;
      startHeight = this.container.offsetHeight;
      const onMove = (ev: MouseEvent) => {
        const dy = startY - ev.clientY;
        const h = Math.max(150, Math.min(window.innerHeight * 0.7, startHeight + dy));
        store.set('terminalHeight', h);
      };
      const onUp = () => {
        document.removeEventListener('mousemove', onMove);
        document.removeEventListener('mouseup', onUp);
        document.body.style.cursor = '';
      };
      document.body.style.cursor = 'ns-resize';
      document.addEventListener('mousemove', onMove);
      document.addEventListener('mouseup', onUp);
    });
  }

  /** Write to the active terminal's PTY */
  executeCommand(cmd: string): void {
    const active = this.getActive();
    if (active && active.ptyId >= 0) {
      forge.terminal.write(active.ptyId, cmd + '\n');
    }
  }

  /** Write directly to the first terminal (build/run output) */
  writeOutput(data: string): void {
    const target = this.instances[0] || this.getActive();
    if (target) {
      target.terminal.write(data.replace(/\r?\n/g, '\r\n'));
    }
  }

  focus(): void {
    this.getActive()?.terminal.focus();
  }

  dispose(): void {
    for (const inst of this.instances) {
      if (inst.ptyId >= 0) forge.terminal.destroy(inst.ptyId);
      inst.terminal.dispose();
    }
  }
}
