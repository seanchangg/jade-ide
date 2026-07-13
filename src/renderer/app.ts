import './styles/main.css';
import 'xterm/css/xterm.css';

import { store, initializeState, loadPersistedState, enableAutoSave } from './state';
import { EditorManager } from './editor/editor-manager';
import { registerLspProviders } from './editor/lsp-providers';
import { initMemoryDecorations, resetMemoryTracking } from './editor/memory-decorations';
import { initExecutionFlow } from './editor/execution-flow';
import { registerAsmLanguage } from './editor/asm-language';
import { registerMetalLanguage } from './editor/metal-language';
import { registerFrequencyCompletion } from './editor/frequency-completion';
import { registerInlineCompletion } from './editor/inline-completion';
import { mountXpBar, attachXpToEditor } from './editor/xp-bar';
import { initDebugDecorations, getAllBreakpoints, setCurrentDebugLine } from './editor/debug-decorations';
import { DebugPanel } from './panels/debug-panel';
import { initStickyNotes } from './editor/sticky-notes';
import { FileTree } from './panels/file-tree';
import { TerminalPanel } from './panels/terminal-panel';
import { MemoryBar } from './panels/memory-bar';
import { initSystemGauges } from './gauges/system-gauges';
import { RuntimePanel } from './panels/runtime-panel';
import { StructurePanel } from './panels/structure-panel';
import { TrainingView } from './ml/training-view';
import { icon } from './ui/icons';
import { button, badge, setBadgeText, setButtonLabel } from './ui/components';

const forge = (window as any).forge;

let editorManager: EditorManager;
let fileTree: FileTree;
let terminalPanel: TerminalPanel;
let memoryBar: MemoryBar;
let runtimePanel: RuntimePanel;
let debugPanel: DebugPanel;

// Quick open state
let quickOpenOverlay: HTMLElement | null = null;
let cachedFileList: { name: string; path: string }[] | null = null;
let quickOpenSelectedIndex = 0;

async function init(): Promise<void> {
  try {
  initializeState();

  const app = document.getElementById('app')!;
  app.innerHTML = '';

  // ── Layout structure ──
  // Top: optional traffic light spacer
  // Main: [file-tree] [editor-area] [training-view]
  // Bottom: terminal, then memory-bar

  // Drag-to-open workspace if nothing is open
  const welcomeOverlay = document.createElement('div');
  welcomeOverlay.id = 'welcome-overlay';
  welcomeOverlay.innerHTML = `
    <div class="welcome-content">
      <div class="welcome-title">Jade</div>
      <div class="welcome-subtitle">Open a folder to get started</div>
      <div id="welcome-btn-slot"></div>
      <div class="welcome-shortcuts">
        <span>⌘B File tree</span>
        <span>⌘\` Terminal</span>
        <span>⌘E Flow arrows</span>
        <span>⌘S Save</span>
      </div>
    </div>
  `;
  app.appendChild(welcomeOverlay);

  // Open-folder button (shadcn outline button + folder icon)
  const openFolderBtn = button({
    id: 'open-folder-btn',
    label: 'Open Folder',
    icon: 'folder-open',
    variant: 'outline',
  });
  document.getElementById('welcome-btn-slot')!.appendChild(openFolderBtn);

  // Register open folder handler immediately (before any async work)
  document.getElementById('open-folder-btn')!.addEventListener('click', openWorkspace);

  // ── Top bar — full width, includes traffic-light spacer and all controls ──
  const actionBar = document.createElement('div');
  actionBar.id = 'action-bar';
  actionBar.innerHTML = `
    <div class="action-group">
      <button class="action-btn action-icon" id="btn-sidebar" title="Toggle sidebar (⌘B)">${icon('panel-left')}</button>
      <button class="action-btn action-icon" id="btn-terminal" title="Toggle terminal (⌘\`)">${icon('terminal')}</button>
      <button class="action-btn action-icon" id="btn-flow" title="Toggle flow arrows (⌘E)">${icon('corner-down-right')}</button>
      <button class="action-btn action-icon" id="btn-runtime" title="Toggle runtime panel">${icon('gauge')}</button>
    </div>
    <div class="action-group diag-summary" id="diag-summary"></div>
    <div class="action-group">
      <select class="action-preset" id="flag-preset" title="Flag presets">
        <option value="">preset</option>
        <option value="-x objective-c++ -Imetal-cpp -framework Metal -framework Foundation -framework QuartzCore">Metal</option>
        <option value="-fopenmp">OpenMP</option>
        <option value="-pthread">pthreads</option>
        <option value="-framework Accelerate">Accelerate</option>
      </select>
      <input class="action-flags" id="custom-flags" placeholder="flags" title="Extra compiler flags (e.g. -DFOO -lm)" spellcheck="false" />
      <button class="action-btn" id="btn-asm" title="Show assembly (⌘⇧A)">${icon('code')}<span class="ui-btn-label">ASM</span></button>
      <button class="action-btn action-build" id="btn-build" title="Build (⌘⇧B)">${icon('hammer')}<span class="ui-btn-label">Build</span></button>
      <button class="action-btn action-run" id="btn-run" title="Run (⌘R)">${icon('play')}<span class="ui-btn-label">Run</span></button>
      <button class="action-btn action-debug" id="btn-debug" title="Debug (⌘⇧D)">${icon('bug')}<span class="ui-btn-label">Debug</span></button>
      <button class="action-btn action-stop" id="btn-stop" title="Stop (⌘.)">${icon('square')}<span class="ui-btn-label">Stop</span></button>
      <button class="action-btn action-icon" id="btn-ai" title="AI completion">${icon('sparkles')}</button>
      <button class="action-btn action-icon" id="btn-theme" title="Toggle light/dark theme">${icon('moon')}</button>
      <button class="action-btn action-icon" id="btn-home" title="Close workspace — return to start">${icon('house')}</button>
    </div>
  `;

  // Diagnostics counts as shadcn badges (icon + tabular count)
  const diagSummary = actionBar.querySelector('#diag-summary')!;
  diagSummary.appendChild(badge({ id: 'diag-errors', icon: 'circle-x', label: '0', variant: 'destructive', className: 'diag-count', title: 'Errors' }));
  diagSummary.appendChild(badge({ id: 'diag-warnings', icon: 'triangle-alert', label: '0', variant: 'warning', className: 'diag-count', title: 'Warnings' }));
  diagSummary.appendChild(badge({ id: 'diag-info', icon: 'info', label: '0', variant: 'secondary', className: 'diag-count', title: 'Info' }));

  actionBar.style.display = 'none';
  app.appendChild(actionBar);

  // Main layout
  const mainLayout = document.createElement('div');
  mainLayout.id = 'main-layout';
  mainLayout.style.display = 'none';

  // File tree sidebar
  const sidebarArea = document.createElement('div');
  sidebarArea.id = 'sidebar-area';
  mainLayout.appendChild(sidebarArea);

  // Center: editor
  const editorArea = document.createElement('div');
  editorArea.id = 'editor-area';
  mainLayout.appendChild(editorArea);

  // Right: runtime sidebar
  const runtimeArea = document.createElement('div');
  runtimeArea.id = 'runtime-area';
  mainLayout.appendChild(runtimeArea);

  app.appendChild(mainLayout);

  // Terminal at bottom
  const terminalArea = document.createElement('div');
  terminalArea.id = 'terminal-area';
  app.appendChild(terminalArea);

  // Memory bar at very bottom
  const memoryBarArea = document.createElement('div');
  memoryBarArea.id = 'memory-bar-area';
  app.appendChild(memoryBarArea);

  // ── Initialize components ──

  editorManager = new EditorManager(editorArea);

  // Sidebar tab switcher (Files | Structure)
  const sidebarTabs = document.createElement('div');
  sidebarTabs.className = 'sidebar-tabs';
  const filesTab = document.createElement('div');
  filesTab.className = 'sidebar-tab active';
  filesTab.textContent = 'FILES';
  filesTab.addEventListener('click', () => {
    filesTab.classList.add('active');
    structTab.classList.remove('active');
    fileTreeWrapper.style.display = 'flex';
    structWrapper.style.display = 'none';
  });
  const structTab = document.createElement('div');
  structTab.className = 'sidebar-tab';
  structTab.textContent = 'STRUCTURE';
  structTab.addEventListener('click', () => {
    structTab.classList.add('active');
    filesTab.classList.remove('active');
    structWrapper.style.display = 'flex';
    fileTreeWrapper.style.display = 'none';
    structurePanel.refresh();
  });
  sidebarTabs.appendChild(filesTab);
  sidebarTabs.appendChild(structTab);
  sidebarArea.insertBefore(sidebarTabs, sidebarArea.firstChild);

  // Wrappers for file tree and structure panels
  const fileTreeWrapper = document.createElement('div');
  fileTreeWrapper.style.cssText = 'display:flex;flex-direction:column;flex:1;overflow:hidden;';
  sidebarArea.appendChild(fileTreeWrapper);

  const structWrapper = document.createElement('div');
  structWrapper.style.cssText = 'display:none;flex-direction:column;flex:1;overflow:hidden;';
  sidebarArea.appendChild(structWrapper);

  fileTree = new FileTree(fileTreeWrapper, (path) => {
    editorManager.openFile(path);
  });

  terminalPanel = new TerminalPanel(terminalArea);
  memoryBar = new MemoryBar(memoryBarArea);
  runtimePanel = new RuntimePanel(runtimeArea);

  // Editor features
  const editor = editorManager.getEditor()!;

  // XP bar — mount into the right side of the tab bar
  mountXpBar(editorManager.getTabBarRight());
  attachXpToEditor(editor);

  // Structure panel (needs editor reference)
  const structurePanel = new StructurePanel(structWrapper, editor);

  initMemoryDecorations(editor);
  initExecutionFlow(editor, editorArea);
  initDebugDecorations(editor);
  initStickyNotes(editor, editorArea);
  registerLspProviders();

  // Debug tool window docks in the bottom (terminal) area, CLion-style
  debugPanel = new DebugPanel(terminalArea);
  const trainingView = new TrainingView(runtimeArea);

  // Auto-show training view when scalar/timing data arrives
  let trainingAutoShown = false;
  forge.build.onTrainingScalar(() => {
    if (!trainingAutoShown) {
      trainingAutoShown = true;
      store.set('runtimeVisible', true);
      store.set('trainingViewVisible', true);
    }
  });
  forge.build.onTrainingTiming(() => {
    if (!trainingAutoShown) {
      trainingAutoShown = true;
      store.set('runtimeVisible', true);
      store.set('trainingViewVisible', true);
    }
  });
  // Reset auto-show flag when a new build starts
  // Clear training data on new run (not on build — build doesn't produce training data)
  store.on('isRunning', (running: boolean) => {
    if (running) {
      trainingAutoShown = false;
      trainingView.clear();
    }
  });

  // System gauges
  initSystemGauges();

  // Memory overlay widget
  // Runtime panel is initialized above

  // Expose editor globally for hotspot click-to-jump
  (window as any).__forgeEditor = editorManager.getEditor();

  // ── Assembly viewer (Godbolt-style) ──
  registerAsmLanguage();
  registerMetalLanguage();
  registerFrequencyCompletion();
  registerInlineCompletion();

  // ── AI completion status → store + action-bar indicator ──
  function updateAiButton(): void {
    const btn = document.getElementById('btn-ai');
    if (!btn) return;
    const state = store.get<string>('aiStatus');
    const detail = store.get<string>('aiStatusDetail') || '';
    const enabled = store.get<boolean>('aiCompletionEnabled') !== false;

    btn.classList.toggle('active', enabled && state === 'ready');
    btn.classList.toggle('ai-starting', enabled && state === 'starting');
    btn.classList.toggle('ai-off', !enabled || state === 'disabled' || state === 'error');

    if (!enabled) btn.title = 'AI completion: off (click to enable)';
    else if (state === 'ready') btn.title = `AI completion: on — ${detail}`;
    else btn.title = `AI completion: ${state}${detail ? ' — ' + detail : ''}`;
  }

  forge.ai.onStatusChanged((s: any) => {
    store.set('aiStatus', s.state);
    store.set('aiStatusDetail', s.detail);
  });
  forge.ai.status().then((s: any) => {
    store.set('aiStatus', s.state);
    store.set('aiStatusDetail', s.detail);
  });
  store.on('aiStatus', updateAiButton);
  store.on('aiCompletionEnabled', updateAiButton);

  // Toggling off kills the model server so it releases GPU/unified memory
  // (important when benchmarking kernels); toggling on restarts it.
  store.on('aiCompletionEnabled', (enabled: boolean) => {
    forge.ai.setEnabled(enabled !== false);
  });

  // Model tier and suggestion length are global preferences
  store.on('aiModel', (id: string) => {
    forge.ai.setModel(id);
    forge.state.save('aiModel', id);
  });
  store.on('aiMultiline', (v: boolean) => {
    forge.state.save('aiMultiline', v === true);
  });
  const asmContainer = document.createElement('div');
  asmContainer.id = 'asm-container';
  editorArea.appendChild(asmContainer);

  let asmEditor: any = null;
  let asmVisible = false;
  let asmToSource: Record<number, number> = {};
  let sourceToAsm: Map<number, number[]> = new Map();
  let asmHighlightIds: string[] = [];
  let srcHighlightIds: string[] = [];
  let suppressCrossHighlight = false;

  async function toggleAsm(): Promise<void> {
    if (asmVisible) {
      asmVisible = false;
      asmContainer.style.display = 'none';
      document.getElementById('btn-asm')!.classList.remove('active');
      // Clear highlights
      if (asmEditor) asmHighlightIds = asmEditor.deltaDecorations(asmHighlightIds, []);
      srcHighlightIds = editor.deltaDecorations(srcHighlightIds, []);
      requestAnimationFrame(() => editorManager.layout());
      return;
    }

    const filePath = editorManager.getActiveFilePath();
    if (!filePath) return;

    asmVisible = true;
    asmContainer.style.display = 'flex';
    document.getElementById('btn-asm')!.classList.add('active');

    // Create asm editor on first use
    if (!asmEditor) {
      asmEditor = monaco.editor.create(asmContainer, {
        value: '; Generating assembly...',
        language: 'asm',
        readOnly: true,
        automaticLayout: true,
        minimap: { enabled: false },
        lineNumbers: 'off',
        glyphMargin: false,
        folding: false,
        scrollBeyondLastLine: false,
        renderLineHighlight: 'none',
        fontFamily: "'JetBrains Mono', 'SF Mono', Menlo, 'Courier New', monospace",
        fontSize: 13,
        theme: 'forge-dark',
      });

      // Cross-highlight: asm cursor → source highlight
      asmEditor.onDidChangeCursorPosition((e: any) => {
        if (suppressCrossHighlight || !asmVisible) return;
        const asmLine = e.position.lineNumber;
        // Find source line — try exact match, then scan nearby lines
        let srcLine = asmToSource[asmLine];
        if (!srcLine) {
          for (let delta = 1; delta <= 5; delta++) {
            srcLine = asmToSource[asmLine - delta] || asmToSource[asmLine + delta];
            if (srcLine) break;
          }
        }
        if (srcLine && srcLine > 0) {
          suppressCrossHighlight = true;
          srcHighlightIds = editor.deltaDecorations(srcHighlightIds, [{
            range: new monaco.Range(srcLine, 1, srcLine, 1),
            options: { isWholeLine: true, className: 'asm-cross-highlight' },
          }]);
          suppressCrossHighlight = false;
        } else {
          srcHighlightIds = editor.deltaDecorations(srcHighlightIds, []);
        }
      });
    }

    requestAnimationFrame(() => {
      editorManager.layout();
      asmEditor.layout();
    });

    await refreshAsm(filePath);
  }

  // Cross-highlight: source cursor → asm highlight
  editor.onDidChangeCursorPosition((e) => {
    if (suppressCrossHighlight || !asmVisible || !asmEditor) return;
    const srcLine = e.position.lineNumber;
    const asmLines = sourceToAsm.get(srcLine);
    if (asmLines && asmLines.length > 0) {
      suppressCrossHighlight = true;
      asmHighlightIds = asmEditor.deltaDecorations(asmHighlightIds, asmLines.map((ln: number) => ({
        range: new monaco.Range(ln, 1, ln, 1),
        options: { isWholeLine: true, className: 'asm-cross-highlight' },
      })));
      // Scroll to first matching asm line
      asmEditor.revealLineInCenter(asmLines[0]);
      suppressCrossHighlight = false;
    } else {
      asmHighlightIds = asmEditor.deltaDecorations(asmHighlightIds, []);
    }
  });

  async function refreshAsm(filePath: string): Promise<void> {
    if (!asmEditor) return;
    asmEditor.setValue('; Generating assembly...');

    const customFlags = (document.getElementById('custom-flags') as HTMLInputElement)?.value.trim();
    const extraFlags = customFlags ? customFlags.split(/\s+/) : [];

    const result = await forge.build.getAsm(filePath, extraFlags);
    if (result.success) {
      asmEditor.setValue(result.asm);
      const model = asmEditor.getModel();
      if (model) monaco.editor.setModelLanguage(model, 'asm');

      // Build bidirectional mapping
      asmToSource = result.asmToSource || {};
      sourceToAsm = new Map();
      for (const [asmLine, srcLine] of Object.entries(asmToSource)) {
        const sl = srcLine as number;
        if (!sourceToAsm.has(sl)) sourceToAsm.set(sl, []);
        sourceToAsm.get(sl)!.push(parseInt(asmLine));
      }
    } else {
      asmEditor.setValue('; Compilation failed:\n; ' + (result.error || 'unknown error').replace(/\n/g, '\n; '));
      asmToSource = {};
      sourceToAsm = new Map();
    }
  }

  // Auto-refresh asm when source changes (debounced)
  let asmRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  editor.onDidChangeModelContent(() => {
    if (!asmVisible) return;
    if (asmRefreshTimer) clearTimeout(asmRefreshTimer);
    asmRefreshTimer = setTimeout(() => {
      const filePath = editorManager.getActiveFilePath();
      if (filePath) {
        // Save first so compiler sees latest
        editorManager.saveActiveFile().then(() => refreshAsm(filePath));
      }
    }, 1500);
  });

  // ── Panel width sync ──
  store.on('fileTreeVisible', (visible: boolean) => {
    sidebarArea.style.width = visible ? '260px' : '28px';
    sidebarArea.classList.toggle('collapsed', !visible);
    requestAnimationFrame(() => editorManager.layout());
  });

  // Click the minimized sidebar strip to reopen
  sidebarArea.addEventListener('click', (e) => {
    if (!store.get<boolean>('fileTreeVisible') && (e.target as HTMLElement).id === 'sidebar-area') {
      store.set('fileTreeVisible', true);
    }
  });

  store.on('runtimeVisible', (visible: boolean) => {
    runtimeArea.style.width = visible ? '280px' : '0';
    requestAnimationFrame(() => editorManager.layout());
  });

  store.on('terminalVisible', () => {
    requestAnimationFrame(() => editorManager.layout());
  });

  // Connect build output to terminal panel
  forge.build.onOutput((data: string) => {
    terminalPanel.writeOutput(data);
  });

  // ── Debug event wiring ──
  forge.debug.onStopped((event: any) => {
    store.set('debugState', 'paused');
    setCurrentDebugLine(event.line);
    debugPanel.show();
    debugPanel.updateLocals(event.locals || []);
    debugPanel.updateCallStack(event.frames || []);
    const where = event.file ? `${event.file.split('/').pop()}:${event.line}` : `line ${event.line}`;
    debugPanel.setStatus(`⏸ Paused at ${where} — ${event.reason || 'stopped'}`, 'paused');
  });

  forge.debug.onExited((code: number) => {
    store.set('debugState', 'idle');
    setCurrentDebugLine(null);
    debugPanel.hide();
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[33mDebug session ended (exit ${code})\x1b[0m\r\n`);
  });

  forge.debug.onOutput((data: string) => {
    debugPanel.appendConsoleOutput(data);
  });

  // ── Action bar button handlers ──
  document.getElementById('btn-sidebar')!.addEventListener('click', () => {
    store.update<boolean>('fileTreeVisible', (v) => !v);
  });
  document.getElementById('btn-terminal')!.addEventListener('click', () => {
    store.update<boolean>('terminalVisible', (v) => !v);
  });
  document.getElementById('btn-flow')!.addEventListener('click', () => {
    store.update<boolean>('executionFlowVisible', (v) => !v);
  });
  document.getElementById('btn-runtime')!.addEventListener('click', () => {
    store.update<boolean>('runtimeVisible', (v) => !v);
  });
  document.getElementById('flag-preset')!.addEventListener('change', (e) => {
    const select = e.target as HTMLSelectElement;
    const flagsInput = document.getElementById('custom-flags') as HTMLInputElement;
    if (select.value) {
      flagsInput.value = select.value;
      select.selectedIndex = 0; // reset dropdown to "preset" label
    }
  });
  document.getElementById('btn-asm')!.addEventListener('click', () => toggleAsm());
  document.getElementById('btn-build')!.addEventListener('click', () => buildActiveFile());
  document.getElementById('btn-run')!.addEventListener('click', () => runLastBuild());
  document.getElementById('btn-debug')!.addEventListener('click', () => debugActiveFile());
  document.getElementById('btn-stop')!.addEventListener('click', () => {
    forge.build.stop();
    forge.debug.stop();
    store.set('isRunning', false);
    store.set('debugState', 'idle');
    setCurrentDebugLine(null);
    debugPanel.hide();
  });
  document.getElementById('btn-home')!.addEventListener('click', () => returnToHome());
  document.getElementById('btn-theme')!.addEventListener('click', () => toggleTheme());
  // ── AI settings menu (sparkle button) ──
  let aiMenu: HTMLElement | null = null;
  function dismissAiMenu(): void {
    if (aiMenu) { aiMenu.remove(); aiMenu = null; }
  }
  document.getElementById('btn-ai')!.addEventListener('click', (e) => {
    e.stopPropagation();
    if (aiMenu) { dismissAiMenu(); return; }

    const enabled = store.get<boolean>('aiCompletionEnabled') !== false;
    const multiline = store.get<boolean>('aiMultiline') === true;
    const modelId = store.get<string>('aiModel') || 'fast';
    const state = store.get<string>('aiStatus');
    const detail = store.get<string>('aiStatusDetail') || '';
    // Model choice only applies to the server Jade manages itself
    const external = state === 'ready' && detail.startsWith('Connected to');

    aiMenu = document.createElement('div');
    aiMenu.className = 'ai-menu';
    aiMenu.addEventListener('click', (ev) => ev.stopPropagation());
    aiMenu.innerHTML = `
      <label class="ai-menu-row">
        <input type="checkbox" id="ai-opt-enabled" ${enabled ? 'checked' : ''}>
        <span>AI completion</span>
      </label>
      <label class="ai-menu-row" title="Off = one line at a time, like CLion. On = up to 6-line blocks.">
        <input type="checkbox" id="ai-opt-multiline" ${multiline ? 'checked' : ''}>
        <span>Multi-line suggestions</span>
      </label>
      <div class="ai-menu-row">
        <span>Model</span>
        <select id="ai-opt-model" ${external ? 'disabled title="Model is chosen by your external server"' : ''}>
          <option value="fast" ${modelId === 'fast' ? 'selected' : ''}>Fast — Qwen2.5-Coder 1.5B</option>
          <option value="balanced" ${modelId === 'balanced' ? 'selected' : ''}>Balanced — 3B (~3.3 GB)</option>
          <option value="best" ${modelId === 'best' ? 'selected' : ''}>Best — 7B (~8 GB)</option>
        </select>
      </div>
      <div class="ai-menu-status" id="ai-menu-status">${state}${detail ? ' — ' + detail : ''}</div>
    `;

    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    aiMenu.style.top = `${rect.bottom + 4}px`;
    aiMenu.style.right = `${window.innerWidth - rect.right}px`;
    document.body.appendChild(aiMenu);

    aiMenu.querySelector('#ai-opt-enabled')!.addEventListener('change', (ev) => {
      store.set('aiCompletionEnabled', (ev.target as HTMLInputElement).checked);
    });
    aiMenu.querySelector('#ai-opt-multiline')!.addEventListener('change', (ev) => {
      store.set('aiMultiline', (ev.target as HTMLInputElement).checked);
    });
    aiMenu.querySelector('#ai-opt-model')!.addEventListener('change', (ev) => {
      store.set('aiModel', (ev.target as HTMLSelectElement).value);
    });
  });
  document.addEventListener('click', dismissAiMenu);

  // Keep the status footer live while the menu is open (model switches take a while)
  store.on('aiStatusDetail', () => {
    const el = document.getElementById('ai-menu-status');
    if (el) {
      const state = store.get<string>('aiStatus');
      const detail = store.get<string>('aiStatusDetail') || '';
      el.textContent = `${state}${detail ? ' — ' + detail : ''}`;
    }
  });

  // Load persisted theme preference
  const savedTheme = await forge.state.load('theme');
  if (savedTheme === 'light') applyTheme('light');

  // Load persisted AI preferences (global, like theme)
  const savedAiModel = await forge.state.load('aiModel');
  if (savedAiModel) store.set('aiModel', savedAiModel);
  const savedAiMultiline = await forge.state.load('aiMultiline');
  if (savedAiMultiline != null) store.set('aiMultiline', savedAiMultiline);

  // ── Monaco keybindings (these work even when editor has focus) ──
  const monaco = await import('monaco-editor');
  const ed = editorManager.getEditor()!;

  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyB, () => {
    store.update<boolean>('fileTreeVisible', (v) => !v);
    requestAnimationFrame(() => editorManager.layout());
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Backquote, () => {
    store.update<boolean>('terminalVisible', (v) => !v);
    requestAnimationFrame(() => editorManager.layout());
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyE, () => {
    store.update<boolean>('executionFlowVisible', (v) => !v);
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyB, () => {
    buildActiveFile();
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyR, () => {
    runLastBuild();
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Period, () => {
    forge.build.stop();
    forge.debug.stop();
    store.set('isRunning', false);
    store.set('debugState', 'idle');
    setCurrentDebugLine(null);
    debugPanel.hide();
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyA, () => {
    toggleAsm();
  });
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyD, () => {
    debugActiveFile();
  });
  // Debug stepping shortcuts
  ed.addCommand(monaco.KeyCode.F5, () => {
    if (store.get('debugState') === 'paused') forge.debug.continue();
  });
  ed.addCommand(monaco.KeyCode.F10, () => {
    if (store.get('debugState') === 'paused') forge.debug.stepOver();
  });
  ed.addCommand(monaco.KeyCode.F11, () => {
    if (store.get('debugState') === 'paused') forge.debug.stepInto();
  });
  ed.addCommand(monaco.KeyMod.Shift | monaco.KeyCode.F11, () => {
    if (store.get('debugState') === 'paused') forge.debug.stepOut();
  });

  // Diagnostic summary — update counts when markers change
  let cachedMarkers: any[] = [];
  function updateDiagCounts() {
    const model = editorManager.getEditor()?.getModel();
    if (!model) return;
    const markers = monaco.editor.getModelMarkers({ resource: model.uri });
    cachedMarkers = markers;
    let errors = 0, warnings = 0, infos = 0;
    for (const m of markers) {
      if (m.severity === monaco.MarkerSeverity.Error) errors++;
      else if (m.severity === monaco.MarkerSeverity.Warning) warnings++;
      else infos++;
    }
    setBadgeText(document.getElementById('diag-errors'), String(errors));
    setBadgeText(document.getElementById('diag-warnings'), String(warnings));
    setBadgeText(document.getElementById('diag-info'), String(infos));
  }
  // Poll for marker changes (Monaco doesn't have a global onDidChangeMarkers in all versions)
  setInterval(updateDiagCounts, 2000);
  // Also update on model change
  ed.onDidChangeModel(updateDiagCounts);

  // Diagnostic dropdown — click summary to show list of issues
  let diagDropdown: HTMLElement | null = null;
  function dismissDiagDropdown() {
    if (diagDropdown) { diagDropdown.remove(); diagDropdown = null; }
  }
  document.getElementById('diag-summary')!.style.cursor = 'pointer';
  document.getElementById('diag-summary')!.addEventListener('click', (e) => {
    e.stopPropagation();
    if (diagDropdown) { dismissDiagDropdown(); return; }

    updateDiagCounts();
    if (cachedMarkers.length === 0) return;

    diagDropdown = document.createElement('div');
    diagDropdown.className = 'diag-dropdown';

    // Position below the summary
    const summaryEl = document.getElementById('diag-summary')!;
    const rect = summaryEl.getBoundingClientRect();
    diagDropdown.style.top = `${rect.bottom + 4}px`;
    diagDropdown.style.right = `${window.innerWidth - rect.right}px`;

    // Sort: errors first, then warnings, then info
    const sorted = [...cachedMarkers].sort((a, b) => b.severity - a.severity);

    for (const m of sorted) {
      const row = document.createElement('div');
      row.className = 'diag-dropdown-row';

      const iconSpan = document.createElement('span');
      iconSpan.className = 'diag-dropdown-icon';
      if (m.severity === monaco.MarkerSeverity.Error) {
        iconSpan.innerHTML = icon('circle-x', 13);
        iconSpan.style.color = 'var(--danger)';
      } else if (m.severity === monaco.MarkerSeverity.Warning) {
        iconSpan.innerHTML = icon('triangle-alert', 13);
        iconSpan.style.color = 'var(--warning)';
      } else {
        iconSpan.innerHTML = icon('info', 13);
        iconSpan.style.color = 'var(--accent2)';
      }
      row.appendChild(iconSpan);

      const line = document.createElement('span');
      line.className = 'diag-dropdown-line';
      line.textContent = `L${m.startLineNumber}`;
      row.appendChild(line);

      const msg = document.createElement('span');
      msg.className = 'diag-dropdown-msg';
      msg.textContent = m.message;
      row.appendChild(msg);

      row.addEventListener('click', () => {
        const editor = editorManager.getEditor();
        if (editor) {
          editor.revealLineInCenter(m.startLineNumber);
          editor.setPosition({ lineNumber: m.startLineNumber, column: m.startColumn || 1 });
          editor.focus();
        }
        dismissDiagDropdown();
      });

      diagDropdown.appendChild(row);
    }

    document.body.appendChild(diagDropdown);
  });
  document.addEventListener('click', dismissDiagDropdown);

  // Quick open (⌘P)
  ed.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyP, () => {
    showQuickOpen();
  });

  // Enable auto-save (starts watching for changes)
  enableAutoSave();

  // Check if we have a workspace to restore
  const savedWorkspace = await forge.state.load('workspaceRoot');
  if (savedWorkspace) {
    await doOpenWorkspace(savedWorkspace);
  }
  } catch (err: any) {
    console.error('[forge] init() failed:', err);
  }
}

async function openWorkspace(): Promise<void> {
  const folderPath = await forge.workspace.openFolderDialog();
  if (!folderPath) return;
  await doOpenWorkspace(folderPath);
}

async function applyTheme(mode: 'light' | 'dark'): Promise<void> {
  const monaco = await import('monaco-editor');
  const themeName = mode === 'light' ? 'forge-light' : 'forge-dark';
  monaco.editor.setTheme(themeName);
  document.body.classList.toggle('light-mode', mode === 'light');
  // Canvas-rendered surfaces (xterm terminals) can't follow CSS variables —
  // publish the mode so they can swap their palettes programmatically.
  // (After the class toggle: subscribers read it to resolve the theme.)
  store.set('themeMode', mode);
  const btn = document.getElementById('btn-theme');
  if (btn) btn.innerHTML = mode === 'light' ? icon('sun') : icon('moon');
}

async function toggleTheme(): Promise<void> {
  const isLight = document.body.classList.contains('light-mode');
  const next = isLight ? 'dark' : 'light';
  await applyTheme(next);
  forge.state.save('theme', next);
}

function returnToHome(): void {
  // Stop anything running
  try { forge.build.stop(); } catch {}
  try { forge.debug.stop(); } catch {}

  // Clear the persisted workspace so next launch doesn't auto-open it
  forge.state.save('workspaceRoot', '');
  store.set('workspaceRoot', '');
  store.set('activeFilePath', '');
  store.set('isRunning', false);
  store.set('debugState', 'idle');

  // Hide main UI, show welcome overlay
  document.getElementById('main-layout')!.style.display = 'none';
  document.getElementById('action-bar')!.style.display = 'none';
  document.getElementById('welcome-overlay')!.style.display = 'flex';
}

async function doOpenWorkspace(folderPath: string): Promise<void> {
  // Invalidate quick-open file cache for new workspace
  cachedFileList = null;

  store.set('workspaceRoot', folderPath);
  await forge.workspace.open(folderPath);
  await forge.state.save('workspaceRoot', folderPath);

  // Show main layout
  document.getElementById('welcome-overlay')!.style.display = 'none';
  document.getElementById('main-layout')!.style.display = 'flex';
  document.getElementById('action-bar')!.style.display = 'flex';

  // Show file tree and terminal by default when opening a workspace
  store.set('fileTreeVisible', true);
  store.set('terminalVisible', true);

  // Restore persisted UI state (must happen after workspace.open so the state file is accessible)
  await loadPersistedState();

  // Load file tree
  await fileTree.refresh();

  // Initialize LSP
  try {
    await forge.lsp.initialize(folderPath);
    store.set('lspReady', true);
  } catch (err) {
    console.warn('LSP initialization failed (clangd may not be installed):', err);
    store.set('lspReady', false);
  }

  // Restore open tabs, or open a default file
  const savedTabs = store.get<any[]>('openTabs');
  if (savedTabs && savedTabs.length > 0) {
    await editorManager.restoreTabs(savedTabs);
    const activeIdx = store.get<number>('activeTabIndex') || 0;
    editorManager.switchToTab(activeIdx);
  } else {
    // Auto-open first .cpp file in the workspace for a good first impression
    const tree = await forge.workspace.getTree();
    const firstCpp = tree.find((f: any) => !f.isDirectory && f.name.endsWith('.cpp'));
    if (firstCpp) {
      await editorManager.openFile(firstCpp.path);
    }
  }

  // Re-layout
  requestAnimationFrame(() => editorManager.layout());
}

// ── Quick open overlay (⌘P) ──

function flattenTree(nodes: any[]): { name: string; path: string }[] {
  const result: { name: string; path: string }[] = [];
  for (const node of nodes) {
    if (!node.isDirectory) {
      result.push({ name: node.name, path: node.path });
    }
    if (node.children && node.children.length > 0) {
      result.push(...flattenTree(node.children));
    }
  }
  return result;
}

async function showQuickOpen(): Promise<void> {
  // Close if already open
  if (quickOpenOverlay) {
    closeQuickOpen();
    return;
  }

  const editorArea = document.getElementById('editor-area');
  if (!editorArea) return;

  // Cache file list from workspace tree
  if (!cachedFileList) {
    try {
      const tree = await forge.workspace.getTree();
      cachedFileList = flattenTree(tree);
    } catch {
      cachedFileList = [];
    }
  }

  quickOpenSelectedIndex = 0;

  // Create overlay
  quickOpenOverlay = document.createElement('div');
  quickOpenOverlay.id = 'quick-open';

  const input = document.createElement('input');
  input.className = 'quick-open-input';
  input.placeholder = 'Search files by name...';
  input.type = 'text';
  quickOpenOverlay.appendChild(input);

  const resultsList = document.createElement('div');
  resultsList.className = 'quick-open-results';
  quickOpenOverlay.appendChild(resultsList);

  editorArea.appendChild(quickOpenOverlay);
  input.focus();

  function getFilteredFiles(query: string): { name: string; path: string }[] {
    if (!cachedFileList) return [];
    if (!query) return cachedFileList.slice(0, 10);
    const lower = query.toLowerCase();
    return cachedFileList
      .filter((f) => f.name.toLowerCase().includes(lower))
      .slice(0, 10);
  }

  function renderResults(query: string): void {
    const files = getFilteredFiles(query);
    resultsList.innerHTML = '';
    quickOpenSelectedIndex = Math.min(quickOpenSelectedIndex, Math.max(0, files.length - 1));

    files.forEach((file, idx) => {
      const row = document.createElement('div');
      row.className = 'quick-open-row' + (idx === quickOpenSelectedIndex ? ' selected' : '');

      const nameSpan = document.createElement('span');
      nameSpan.textContent = file.name;
      row.appendChild(nameSpan);

      // Show relative path hint
      const workspaceRoot = store.get<string>('workspaceRoot') || '';
      const relPath = file.path.replace(workspaceRoot, '').replace(/^\//, '');
      if (relPath !== file.name) {
        const pathSpan = document.createElement('span');
        pathSpan.className = 'quick-open-row-path';
        pathSpan.textContent = relPath;
        row.appendChild(pathSpan);
      }

      row.addEventListener('click', () => {
        editorManager.openFile(file.path);
        closeQuickOpen();
      });

      resultsList.appendChild(row);
    });
  }

  input.addEventListener('input', () => {
    quickOpenSelectedIndex = 0;
    renderResults(input.value);
  });

  input.addEventListener('keydown', (e: KeyboardEvent) => {
    const files = getFilteredFiles(input.value);
    if (e.key === 'Escape') {
      e.preventDefault();
      closeQuickOpen();
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      quickOpenSelectedIndex = Math.min(quickOpenSelectedIndex + 1, files.length - 1);
      renderResults(input.value);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      quickOpenSelectedIndex = Math.max(quickOpenSelectedIndex - 1, 0);
      renderResults(input.value);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      if (files.length > 0 && quickOpenSelectedIndex < files.length) {
        editorManager.openFile(files[quickOpenSelectedIndex].path);
        closeQuickOpen();
      }
    }
  });

  // Close on click outside
  function handleOutsideClick(e: MouseEvent): void {
    if (quickOpenOverlay && !quickOpenOverlay.contains(e.target as Node)) {
      closeQuickOpen();
    }
  }
  setTimeout(() => document.addEventListener('click', handleOutsideClick, { once: true }), 0);

  // Render initial results
  renderResults('');
}

function closeQuickOpen(): void {
  if (quickOpenOverlay && quickOpenOverlay.parentNode) {
    quickOpenOverlay.parentNode.removeChild(quickOpenOverlay);
  }
  quickOpenOverlay = null;
}

async function buildActiveFile(): Promise<void> {
  const filePath = editorManager.getActiveFilePath();
  if (!filePath) return;

  // Save before building
  await editorManager.saveActiveFile();

  // Update button state
  const btnBuild = document.getElementById('btn-build');
  if (btnBuild) {
    setButtonLabel(btnBuild, 'Building\u2026');
    btnBuild.classList.add('is-loading');
    (btnBuild as HTMLButtonElement).disabled = true;
  }

  store.set('isBuilding', true);
  resetMemoryTracking();

  // Ensure terminal is visible for output
  store.set('terminalVisible', true);

  const fileName = filePath.split('/').pop() || filePath;
  terminalPanel.writeOutput(`\r\n\x1b[36m[forge]\x1b[0m Building ${fileName}...\r\n`);

  // Always enable sanitizers for automatic memory/leak detection
  // The compile handler accepts: (filePath, flags, sanitize, instrument)
  const enableSanitizers = false; // Use malloc interposer instead (ASan conflicts with it)
  const enableInstrumentation = store.get<boolean>('executionFlowVisible') || false;
  const customFlags = (document.getElementById('custom-flags') as HTMLInputElement)?.value.trim();
  const extraFlags = customFlags ? customFlags.split(/\s+/) : [];
  const result = await forge.build.compile(
    filePath,
    extraFlags,
    enableSanitizers,
    enableInstrumentation
  );
  store.set('isBuilding', false);
  store.set('lastBuildResult', result);
  // Stash build options so runLastBuild can pick them up
  store.set('lastBuildSanitize', enableSanitizers);
  store.set('lastBuildInstrument', enableInstrumentation);

  // Restore button state
  if (btnBuild) {
    setButtonLabel(btnBuild, 'Build');
    btnBuild.classList.remove('is-loading');
    (btnBuild as HTMLButtonElement).disabled = false;
  }

  if (result.success) {
    // Clear any previous build error markers
    const monacoMod = await import('monaco-editor');
    const edModel = editorManager.getEditor()?.getModel();
    if (edModel) {
      monacoMod.editor.setModelMarkers(edModel, 'forge-build', []);
    }
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[32mBuild succeeded\x1b[0m (${result.duration}ms)\r\n`);
  } else {
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[31mBuild failed\x1b[0m (${result.errors.length} error(s), ${result.duration}ms)\r\n`);

    // Set Monaco markers for build errors (red squigglies) and jump to first error
    const monacoMod = await import('monaco-editor');
    const ed = editorManager.getEditor();
    const edModel = ed?.getModel();
    if (ed && edModel && result.errors.length > 0) {
      const markers: any[] = [];
      for (const err of result.errors) {
        // Only add markers for errors in the active file
        if (err.file === filePath || err.file.endsWith('/' + (filePath.split('/').pop() || ''))) {
          markers.push({
            severity: err.severity === 'error' ? monacoMod.MarkerSeverity.Error :
                      err.severity === 'warning' ? monacoMod.MarkerSeverity.Warning :
                      monacoMod.MarkerSeverity.Info,
            message: err.message,
            startLineNumber: err.line,
            startColumn: err.column || 1,
            endLineNumber: err.line,
            endColumn: err.column ? err.column + 1 : edModel.getLineMaxColumn(err.line),
          });
        }
      }
      monacoMod.editor.setModelMarkers(edModel, 'forge-build', markers);

      // Jump to the first error
      const firstError = result.errors[0];
      if (firstError) {
        ed.revealLineInCenter(firstError.line);
        ed.setPosition({ lineNumber: firstError.line, column: firstError.column || 1 });
      }
    }
  }
}

async function runLastBuild(): Promise<void> {
  const result = store.get<any>('lastBuildResult');
  if (!result || !result.executable) return;

  // Update button state
  const btnRun = document.getElementById('btn-run');
  if (btnRun) {
    setButtonLabel(btnRun, 'Running\u2026');
    btnRun.classList.add('is-loading');
  }

  store.set('isRunning', true);
  // Don't reset memory tracking here — the heap-summary IPC might arrive after run resolves
  // Reset happens at build time instead

  // Ensure terminal is visible for output
  store.set('terminalVisible', true);

  const execName = result.executable.split('/').pop() || result.executable;
  terminalPanel.writeOutput(`\r\n\x1b[36m[forge]\x1b[0m Running ./${execName}...\r\n`);

  // Use the build-time flags so the run config matches the compiled binary
  const enableSanitizers = store.get<boolean>('lastBuildSanitize') || false;
  const enableInstrumentation = store.get<boolean>('lastBuildInstrument') ||
    store.get<boolean>('executionFlowVisible') || false;

  const runResult = await forge.build.run({
    executable: result.executable,
    enableSanitizers,
    enableInstrumentation,
  });

  // Small delay to let IPC memory events arrive before we stop listening
  await new Promise(r => setTimeout(r, 200));

  store.set('isRunning', false);

  // Restore button state
  if (btnRun) {
    setButtonLabel(btnRun, 'Run');
    btnRun.classList.remove('is-loading');
  }

  if (runResult.exitCode === 0) {
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[32mExited with code 0\x1b[0m (${runResult.duration}ms)\r\n`);
  } else {
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[31mExited with code ${runResult.exitCode}\x1b[0m (${runResult.duration}ms)\r\n`);
  }

  // Record run stats for the overlay
  runtimePanel.recordRun(runResult.duration);
  // Auto-show the runtime sidebar after first run
  store.set('runtimeVisible', true);

  // Show interposer/ASan memory summary in terminal
  if (runResult.interposeActive) {
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m Memory tracked via malloc interposer\r\n`);
  }

  // Update executed lines for flow arrows (Map<lineNum, count>)
  if (runResult.executedLines && typeof runResult.executedLines === 'object') {
    const entries = Object.entries(runResult.executedLines);
    if (entries.length > 0) {
      store.set('executedLines', new Map(entries.map(([k, v]) => [parseInt(k), v as number])));
    }
  }

  // If there was an error, mark the error line
  if (runResult.exitCode !== 0 && runResult.sanitizerOutput) {
    // Try to parse sanitizer output for error location
    const match = runResult.sanitizerOutput.match(/(\S+):(\d+):\d+/);
    if (match) {
      store.set('errorLine', parseInt(match[2], 10));
    }
  }
}

async function debugActiveFile(): Promise<void> {
  const filePath = editorManager.getActiveFilePath();
  if (!filePath) return;

  await editorManager.saveActiveFile();

  const btnDebug = document.getElementById('btn-debug');
  if (btnDebug) {
    setButtonLabel(btnDebug, 'Building…');
    btnDebug.classList.add('is-loading');
  }

  store.set('terminalVisible', true);
  terminalPanel.writeOutput(`\r\n\x1b[36m[forge]\x1b[0m Building for debug...\r\n`);

  // Build with -O0 for best debug experience
  const customFlags = (document.getElementById('custom-flags') as HTMLInputElement)?.value.trim();
  const debugExtraFlags = customFlags ? ['-O0', ...customFlags.split(/\s+/)] : ['-O0'];
  const result = await forge.build.compile(filePath, debugExtraFlags, false, false);

  if (btnDebug) {
    setButtonLabel(btnDebug, 'Debug');
    btnDebug.classList.remove('is-loading');
  }

  if (!result.success) {
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[31mBuild failed — cannot debug\x1b[0m\r\n`);
    return;
  }

  store.set('debugState', 'running');
  store.set('runtimeVisible', true);

  // Show the debug view immediately — controls, locals, and console are live
  // for the whole session, not just after the first breakpoint hit
  debugPanel.show();
  debugPanel.updateLocals([]);
  debugPanel.updateCallStack([]);

  // Run from the executable's directory (the CMake build dir) so runtime
  // artifacts like default.metallib resolve, matching a normal run
  const cwd = result.executable.substring(0, result.executable.lastIndexOf('/'));
  const breakpoints = getAllBreakpoints();

  terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[33mStarting LLDB debugger...\x1b[0m\r\n`);

  try {
    await forge.debug.start(result.executable, cwd, breakpoints);
  } catch (err: any) {
    terminalPanel.writeOutput(`\x1b[36m[forge]\x1b[0m \x1b[31mDebugger failed: ${err.message}\x1b[0m\r\n`);
    store.set('debugState', 'idle');
    debugPanel.hide();
  }
}

// ── Boot ──
document.addEventListener('DOMContentLoaded', init);
