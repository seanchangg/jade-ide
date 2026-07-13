import * as monaco from 'monaco-editor';
import { registerForgeTheme, FORGE_THEME_NAME } from './theme';
import { store } from '../state';
import type { TabState } from '../../shared/types';

const forge = (window as any).forge;

interface OpenTab {
  path: string;
  model: monaco.editor.ITextModel;
  viewState: monaco.editor.ICodeEditorViewState | null;
  version: number;
  isDirty: boolean;
}

export class EditorManager {
  private editor: monaco.editor.IStandaloneCodeEditor | null = null;
  private tabs: OpenTab[] = [];
  private activeIndex = -1;
  private container: HTMLElement;
  private tabBar: HTMLElement;
  private tabList: HTMLElement;
  private tabBarRight: HTMLElement;
  private editorContainer: HTMLElement;

  constructor(parent: HTMLElement) {
    this.container = parent;

    // Create tab bar with left (tabs) and right (xp/extras) sections
    this.tabBar = document.createElement('div');
    this.tabBar.className = 'tab-bar';
    this.tabList = document.createElement('div');
    this.tabList.className = 'tab-list';
    this.tabBarRight = document.createElement('div');
    this.tabBarRight.className = 'tab-bar-right';
    this.tabBar.appendChild(this.tabList);
    this.tabBar.appendChild(this.tabBarRight);
    this.container.appendChild(this.tabBar);

    // Create editor container
    this.editorContainer = document.createElement('div');
    this.editorContainer.className = 'editor-container';
    this.container.appendChild(this.editorContainer);

    // Register theme before creating editor
    registerForgeTheme();

    // Create Monaco instance
    this.editor = monaco.editor.create(this.editorContainer, {
      theme: FORGE_THEME_NAME,
      fontFamily: "'JetBrains Mono', 'SF Mono', Menlo, 'Courier New', monospace",
      fontSize: 13,
      lineHeight: 20,
      fontLigatures: true,
      fontWeight: '400',
      letterSpacing: 0,
      minimap: { enabled: false },
      scrollbar: {
        verticalScrollbarSize: 6,
        horizontalScrollbarSize: 6,
        verticalSliderSize: 6,
        horizontalSliderSize: 6,
        useShadows: false,
      },
      overviewRulerLanes: 3,
      overviewRulerBorder: false,
      hideCursorInOverviewRuler: false,
      renderLineHighlight: 'line',
      renderLineHighlightOnlyWhenFocus: false,
      cursorBlinking: 'smooth',
      cursorSmoothCaretAnimation: 'on',
      smoothScrolling: true,
      padding: { top: 16, bottom: 16 },
      lineNumbers: 'on',
      lineDecorationsWidth: 8,
      glyphMargin: true,
      folding: true,
      foldingHighlight: false,
      bracketPairColorization: { enabled: false },
      guides: {
        indentation: true,
        bracketPairs: false,
        highlightActiveIndentation: true,
      },
      suggest: {
        showIcons: true,
        showStatusBar: false,
        preview: true,
        previewMode: 'subwordSmart',
      },
      inlineSuggest: { enabled: true },
      parameterHints: { enabled: true },
      quickSuggestions: { other: true, comments: false, strings: false },
      wordWrap: 'off',
      automaticLayout: true,
      contextmenu: true,
      dragAndDrop: false,
      links: true,
      colorDecorators: false,
      tabSize: 4,
      insertSpaces: true,
      renderWhitespace: 'none',
      matchBrackets: 'always',
    });

    // Word navigation — stop at punctuation boundaries instead of jumping past
    // Option+Right/Left on Mac (Alt+Right/Left on Linux)
    this.editor.addCommand(monaco.KeyMod.Alt | monaco.KeyCode.RightArrow, () => {
      this.editor!.trigger('keyboard', 'cursorWordEndRight', {});
    });
    this.editor.addCommand(monaco.KeyMod.Alt | monaco.KeyCode.LeftArrow, () => {
      this.editor!.trigger('keyboard', 'cursorWordStartLeft', {});
    });
    this.editor.addCommand(monaco.KeyMod.Alt | monaco.KeyMod.Shift | monaco.KeyCode.RightArrow, () => {
      this.editor!.trigger('keyboard', 'cursorWordEndRightSelect', {});
    });
    this.editor.addCommand(monaco.KeyMod.Alt | monaco.KeyMod.Shift | monaco.KeyCode.LeftArrow, () => {
      this.editor!.trigger('keyboard', 'cursorWordStartLeftSelect', {});
    });

    // Save on Cmd+S
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => {
      this.saveActiveFile();
    });

    // Close tab on Cmd+W
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyW, () => {
      if (this.activeIndex >= 0) {
        this.closeTab(this.activeIndex);
      }
    });

    // Track dirty state on model changes
    this.editor.onDidChangeModelContent(() => {
      if (this.activeIndex >= 0) {
        this.tabs[this.activeIndex].isDirty = true;
        this.tabs[this.activeIndex].version++;
        this.renderTabs();
        this.notifyLspChange();
      }
    });
  }

  async openFile(filePath: string): Promise<void> {
    // Check if already open
    const existingIndex = this.tabs.findIndex((t) => t.path === filePath);
    if (existingIndex >= 0) {
      this.switchToTab(existingIndex);
      return;
    }

    // Read file content
    const content = await forge.workspace.readFile(filePath);
    const uri = monaco.Uri.file(filePath);
    const language = this.detectLanguage(filePath);

    // Create or get model
    let model = monaco.editor.getModel(uri);
    if (model) {
      model.setValue(content);
    } else {
      model = monaco.editor.createModel(content, language, uri);
    }

    // Add tab
    const tab: OpenTab = {
      path: filePath,
      model,
      viewState: null,
      version: 1,
      isDirty: false,
    };

    this.tabs.push(tab);
    this.switchToTab(this.tabs.length - 1);

    // Notify LSP (skip for Metal shaders — clangd doesn't understand MSL)
    if (!filePath.endsWith('.metal')) {
      forge.lsp.didOpen(uri.toString(), content, 1);
    }
  }

  switchToTab(index: number): void {
    if (index < 0 || index >= this.tabs.length) return;

    // Save current view state
    if (this.activeIndex >= 0 && this.editor) {
      this.tabs[this.activeIndex].viewState = this.editor.saveViewState();
    }

    this.activeIndex = index;
    const tab = this.tabs[index];

    if (this.editor) {
      this.editor.setModel(tab.model);
      if (tab.viewState) {
        this.editor.restoreViewState(tab.viewState);
      }
      this.editor.focus();
    }

    store.set('activeFilePath', tab.path);
    store.set('activeTabIndex', index);
    this.renderTabs();
    this.syncTabState();
  }

  closeTab(index: number): void {
    if (index < 0 || index >= this.tabs.length) return;

    const tab = this.tabs[index];
    const wasActive = index === this.activeIndex;

    // Notify LSP
    forge.lsp.didClose(monaco.Uri.file(tab.path).toString());

    // Dispose model
    tab.model.dispose();

    this.tabs.splice(index, 1);

    if (this.tabs.length === 0) {
      // No tabs left
      this.activeIndex = -1;
      if (this.editor) {
        this.editor.setModel(null);
      }
      store.set('activeFilePath', '');
    } else if (wasActive) {
      // Closed the active tab — switch to the nearest tab
      const newIndex = Math.min(index, this.tabs.length - 1);
      this.activeIndex = -1; // force switchToTab to actually switch
      this.switchToTab(newIndex);
    } else if (index < this.activeIndex) {
      // Closed a tab before the active one — adjust index
      this.activeIndex--;
    }

    this.renderTabs();
    this.syncTabState();
  }

  async saveActiveFile(): Promise<void> {
    if (this.activeIndex < 0) return;
    const tab = this.tabs[this.activeIndex];
    if (!tab.isDirty) return;

    const content = tab.model.getValue();
    await forge.workspace.writeFile(tab.path, content);
    tab.isDirty = false;
    this.renderTabs();
    this.syncTabState();
  }

  getEditor(): monaco.editor.IStandaloneCodeEditor | null {
    return this.editor;
  }

  getActiveFilePath(): string {
    return this.activeIndex >= 0 ? this.tabs[this.activeIndex].path : '';
  }

  getActiveModel(): monaco.editor.ITextModel | null {
    return this.activeIndex >= 0 ? this.tabs[this.activeIndex].model : null;
  }

  layout(): void {
    this.editor?.layout();
  }

  // Restore tabs from persisted state
  async restoreTabs(tabStates: TabState[]): Promise<void> {
    for (const ts of tabStates) {
      try {
        await this.openFile(ts.path);
      } catch {
        // File may have been deleted — skip
      }
    }
  }

  private renderTabs(): void {
    this.tabList.innerHTML = '';

    if (this.tabs.length === 0) {
      this.tabBar.style.display = 'none';
      return;
    }

    this.tabBar.style.display = 'flex';

    this.tabs.forEach((tab, i) => {
      const el = document.createElement('div');
      el.className = `tab ${i === this.activeIndex ? 'tab-active' : ''}`;

      // Drag to reorder
      el.draggable = true;
      el.dataset.tabIndex = String(i);
      el.addEventListener('dragstart', (e) => {
        e.dataTransfer!.setData('tab-index', String(i));
        e.dataTransfer!.effectAllowed = 'move';
        el.classList.add('tab-dragging');
      });
      el.addEventListener('dragend', () => {
        el.classList.remove('tab-dragging');
        this.tabBar.querySelectorAll('.tab-drop-target').forEach(t => t.classList.remove('tab-drop-target'));
      });
      el.addEventListener('dragover', (e) => {
        e.preventDefault();
        e.dataTransfer!.dropEffect = 'move';
        this.tabBar.querySelectorAll('.tab-drop-target').forEach(t => t.classList.remove('tab-drop-target'));
        el.classList.add('tab-drop-target');
      });
      el.addEventListener('dragleave', () => {
        el.classList.remove('tab-drop-target');
      });
      el.addEventListener('drop', (e) => {
        e.preventDefault();
        el.classList.remove('tab-drop-target');
        const fromIdx = parseInt(e.dataTransfer!.getData('tab-index'), 10);
        const toIdx = i;
        if (isNaN(fromIdx) || fromIdx === toIdx) return;
        // Reorder tabs array
        const [moved] = this.tabs.splice(fromIdx, 1);
        this.tabs.splice(toIdx, 0, moved);
        // Update active index
        if (this.activeIndex === fromIdx) {
          this.activeIndex = toIdx;
        } else if (fromIdx < this.activeIndex && toIdx >= this.activeIndex) {
          this.activeIndex--;
        } else if (fromIdx > this.activeIndex && toIdx <= this.activeIndex) {
          this.activeIndex++;
        }
        this.renderTabs();
        this.syncTabState();
      });

      // Middle-click to close
      el.addEventListener('auxclick', (e) => {
        if (e.button === 1) { e.preventDefault(); this.closeTab(i); }
      });

      const name = document.createElement('span');
      name.className = 'tab-name';
      name.textContent = tab.path.split('/').pop() || '';
      el.appendChild(name);

      if (tab.isDirty) {
        const dot = document.createElement('span');
        dot.className = 'tab-dirty';
        dot.textContent = '●';
        el.appendChild(dot);
      }

      const close = document.createElement('span');
      close.className = 'tab-close';
      close.textContent = '×';
      close.addEventListener('click', (e) => {
        e.stopPropagation();
        this.closeTab(i);
      });
      el.appendChild(close);

      el.addEventListener('click', () => this.switchToTab(i));
      this.tabList.appendChild(el);
    });
  }

  getTabBarRight(): HTMLElement {
    return this.tabBarRight;
  }

  getEditorInstance(): monaco.editor.IStandaloneCodeEditor | null {
    return this.editor;
  }

  private notifyLspChange(): void {
    if (this.activeIndex < 0 || !this.editor) return;
    const tab = this.tabs[this.activeIndex];
    const uri = monaco.Uri.file(tab.path).toString();
    const content = tab.model.getValue();

    // Full sync for simplicity — incremental sync is an optimization for later
    forge.lsp.didChange(
      uri,
      [{ text: content }],
      tab.version
    );
  }

  private syncTabState(): void {
    const tabStates: TabState[] = this.tabs.map((t) => ({
      path: t.path,
      isDirty: t.isDirty,
    }));
    store.set('openTabs', tabStates);
  }

  private detectLanguage(filePath: string): string {
    const ext = filePath.split('.').pop()?.toLowerCase() || '';
    const map: Record<string, string> = {
      'cpp': 'cpp', 'cc': 'cpp', 'cxx': 'cpp', 'c++': 'cpp',
      'c': 'c', 'h': 'cpp', 'hpp': 'cpp', 'hxx': 'cpp',
      'cu': 'cpp', 'metal': 'metal', 'mm': 'objective-c',  'm': 'objective-c',
      'json': 'json', 'txt': 'plaintext',
      'md': 'markdown', 'cmake': 'plaintext',
      'py': 'python', 'sh': 'shell', 'zsh': 'shell',
      'js': 'javascript', 'ts': 'typescript',
      'makefile': 'plaintext', 'mk': 'plaintext',
    };
    return map[ext] || 'plaintext';
  }
}
