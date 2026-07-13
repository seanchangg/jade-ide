import * as monaco from 'monaco-editor';
import { store } from '../state';
import type { StickyNote } from '../../shared/types';

let noteElements = new Map<string, HTMLElement>();
let editorContainer: HTMLElement;
let editor: monaco.editor.IStandaloneCodeEditor;

export function initStickyNotes(
  ed: monaco.editor.IStandaloneCodeEditor,
  container: HTMLElement
): void {
  editor = ed;
  editorContainer = container;

  // Render existing notes
  store.on('stickyNotes', (notes: StickyNote[]) => {
    renderAllNotes(notes);
  });

  // Update positions on scroll
  editor.onDidScrollChange(() => {
    updatePositions();
  });

  // Left-click on line numbers to create a sticky note
  editor.onMouseDown((e) => {
    if (e.target.type === monaco.editor.MouseTargetType.GUTTER_LINE_NUMBERS) {
      const line = e.target.position?.lineNumber;
      if (line) {
        createNote(line, e.event.posx, e.event.posy);
      }
    }
  });
}

function createNote(line: number, x: number, y: number): void {
  const model = editor.getModel();
  if (!model) return;

  const filePath = model.uri.fsPath;

  // Check if a note already exists on this line
  const existing = store.get<StickyNote[]>('stickyNotes') || [];
  if (existing.some((n) => n.filePath === filePath && n.anchorLine === line)) {
    return; // don't stack notes on the same line
  }

  const note: StickyNote = {
    id: `note-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    filePath,
    anchorLine: line,
    x: Math.min(x, window.innerWidth - 240),
    y: Math.min(y, window.innerHeight - 200),
    width: 220,
    height: 160,
    content: '',
    createdAt: Date.now(),
    updatedAt: Date.now(),
  };

  store.update<StickyNote[]>('stickyNotes', (notes) => [...(notes || []), note]);
}

function renderAllNotes(notes: StickyNote[]): void {
  const model = editor.getModel();
  const currentFile = model?.uri.fsPath || '';

  // Remove notes that no longer exist
  for (const [id, el] of noteElements) {
    if (!notes.find((n) => n.id === id)) {
      el.remove();
      noteElements.delete(id);
    }
  }

  // Create or update notes for current file
  for (const note of notes) {
    if (note.filePath !== currentFile) {
      // Hide notes from other files
      const el = noteElements.get(note.id);
      if (el) el.style.display = 'none';
      continue;
    }

    let el = noteElements.get(note.id);
    if (!el) {
      el = buildNoteElement(note);
      editorContainer.appendChild(el);
      noteElements.set(note.id, el);
    }

    el.style.display = 'block';
    el.style.left = `${note.x}px`;
    el.style.top = `${note.y}px`;
    el.style.width = `${note.width}px`;
    el.style.height = `${note.height}px`;
  }
}

function buildNoteElement(note: StickyNote): HTMLElement {
  const el = document.createElement('div');
  el.className = 'sticky-note';
  el.dataset.noteId = note.id;

  // Header with line anchor and close button
  const header = document.createElement('div');
  header.className = 'sticky-note-header';

  const lineTag = document.createElement('span');
  lineTag.className = 'sticky-note-line';
  lineTag.textContent = `L${note.anchorLine}`;
  header.appendChild(lineTag);

  const closeBtn = document.createElement('span');
  closeBtn.className = 'sticky-note-close';
  closeBtn.textContent = '×';
  closeBtn.addEventListener('click', () => deleteNote(note.id));
  header.appendChild(closeBtn);

  el.appendChild(header);

  // Content area — renders markdown-style checkboxes
  const content = document.createElement('div');
  content.className = 'sticky-note-content';
  content.contentEditable = 'true';
  content.spellcheck = false;

  // Set initial content
  content.innerHTML = markdownToHtml(note.content);

  // Save on input (debounced) + live checkbox conversion
  let saveTimer: ReturnType<typeof setTimeout> | null = null;
  content.addEventListener('input', () => {
    // Convert [ ] and [x] to checkboxes
    // Work on the full innerHTML of the content div since contentEditable
    // may have raw text nodes, <div>s, or <br>s
    const html = content.innerHTML;
    const normalized = html.replace(/&nbsp;/g, ' ');
    if (normalized.includes('[ ]') || normalized.includes('[x]') || normalized.includes('[]')) {
      content.innerHTML = normalized
        .replace(/\[x\]/g, '<span class="checkbox checked">☑</span>')
        .replace(/\[ ?\]/g, '<span class="checkbox">☐</span>');
      // Move cursor to end
      const range = document.createRange();
      range.selectNodeContents(content);
      range.collapse(false);
      const sel = window.getSelection();
      sel?.removeAllRanges();
      sel?.addRange(range);
    }

    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      const text = htmlToMarkdown(content);
      updateNoteContent(note.id, text);
    }, 500);
  });

  // Handle checkbox clicks
  content.addEventListener('click', (e) => {
    const target = e.target as HTMLElement;
    if (target.classList.contains('checkbox')) {
      target.classList.toggle('checked');
      target.textContent = target.classList.contains('checked') ? '☑' : '☐';
      const text = htmlToMarkdown(content);
      updateNoteContent(note.id, text);
    }
  });

  el.appendChild(content);

  // Dragging
  makeDraggable(el, header, note.id);

  // Resizing
  const resizeHandle = document.createElement('div');
  resizeHandle.className = 'sticky-note-resize';
  el.appendChild(resizeHandle);
  makeResizable(el, resizeHandle, note.id);

  return el;
}

function makeDraggable(el: HTMLElement, handle: HTMLElement, noteId: string): void {
  let startX = 0, startY = 0, startLeft = 0, startTop = 0;

  handle.addEventListener('mousedown', (e) => {
    if ((e.target as HTMLElement).classList.contains('sticky-note-close')) return;

    startX = e.clientX;
    startY = e.clientY;
    startLeft = el.offsetLeft;
    startTop = el.offsetTop;

    const onMove = (ev: MouseEvent) => {
      const dx = ev.clientX - startX;
      const dy = ev.clientY - startY;
      el.style.left = `${startLeft + dx}px`;
      el.style.top = `${startTop + dy}px`;
    };

    const onUp = () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      updateNotePosition(noteId, el.offsetLeft, el.offsetTop);
    };

    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}

function makeResizable(el: HTMLElement, handle: HTMLElement, noteId: string): void {
  let startX = 0, startY = 0, startW = 0, startH = 0;

  handle.addEventListener('mousedown', (e) => {
    e.stopPropagation();
    startX = e.clientX;
    startY = e.clientY;
    startW = el.offsetWidth;
    startH = el.offsetHeight;

    const onMove = (ev: MouseEvent) => {
      const w = Math.max(160, startW + ev.clientX - startX);
      const h = Math.max(100, startH + ev.clientY - startY);
      el.style.width = `${w}px`;
      el.style.height = `${h}px`;
    };

    const onUp = () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      updateNoteSize(noteId, el.offsetWidth, el.offsetHeight);
    };

    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}

function deleteNote(id: string): void {
  store.update<StickyNote[]>('stickyNotes', (notes) =>
    (notes || []).filter((n) => n.id !== id)
  );
  const el = noteElements.get(id);
  if (el) {
    el.remove();
    noteElements.delete(id);
  }
}

function updateNoteContent(id: string, content: string): void {
  store.update<StickyNote[]>('stickyNotes', (notes) =>
    (notes || []).map((n) =>
      n.id === id ? { ...n, content, updatedAt: Date.now() } : n
    )
  );
}

function updateNotePosition(id: string, x: number, y: number): void {
  store.update<StickyNote[]>('stickyNotes', (notes) =>
    (notes || []).map((n) =>
      n.id === id ? { ...n, x, y, updatedAt: Date.now() } : n
    )
  );
}

function updateNoteSize(id: string, width: number, height: number): void {
  store.update<StickyNote[]>('stickyNotes', (notes) =>
    (notes || []).map((n) =>
      n.id === id ? { ...n, width, height, updatedAt: Date.now() } : n
    )
  );
}

function updatePositions(): void {
  // Sticky notes are absolute-positioned, so scrolling doesn't move them.
  // They float over the editor. This is intentional — they're "sticky" notes.
}

// ── Markdown ↔ HTML for checkboxes ──

function markdownToHtml(md: string): string {
  if (!md) return '<br>';
  return md
    .split('\n')
    .map((line) => {
      line = line
        .replace(/\[x\]/g, '<span class="checkbox checked">☑</span>')
        .replace(/\[ \]/g, '<span class="checkbox">☐</span>');
      return `<div>${line || '<br>'}</div>`;
    })
    .join('');
}

function htmlToMarkdown(el: HTMLElement): string {
  let html = el.innerHTML;
  // Replace checked checkboxes (class order may vary)
  html = html.replace(/<span class="[^"]*checked[^"]*">[^<]*<\/span>/g, '[x]');
  // Replace unchecked checkboxes
  html = html.replace(/<span class="checkbox">[^<]*<\/span>/g, '[ ]');
  // Convert <div> and <br> to newlines
  html = html.replace(/<div>/gi, '\n');
  html = html.replace(/<\/div>/gi, '');
  html = html.replace(/<br\s*\/?>/gi, '\n');
  // Strip remaining HTML tags
  const tmp = document.createElement('div');
  tmp.innerHTML = html;
  let text = tmp.textContent || '';
  // Also convert any leftover unicode checkmarks to markdown
  text = text.replace(/☑/g, '[x]');
  text = text.replace(/☐/g, '[ ]');
  // Clean up leading newline
  return text.replace(/^\n/, '');
}
