import { store } from '../state';
import type { FileNode } from '../../shared/types';

const forge = (window as any).forge;

type FileOpenCallback = (path: string) => void;

export class FileTree {
  private container: HTMLElement;
  private treeContainer: HTMLElement;
  private onFileOpen: FileOpenCallback;
  private expandedDirs = new Set<string>();
  private contextMenu: HTMLElement | null = null;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(parent: HTMLElement, onFileOpen: FileOpenCallback) {
    this.onFileOpen = onFileOpen;

    this.container = document.createElement('div');
    this.container.className = 'file-tree-panel';

    // Header
    const header = document.createElement('div');
    header.className = 'file-tree-header';

    const title = document.createElement('span');
    title.className = 'file-tree-title';
    title.textContent = 'FILES';
    header.appendChild(title);

    const headerRight = document.createElement('div');
    headerRight.style.display = 'flex';
    headerRight.style.alignItems = 'center';
    headerRight.style.gap = '6px';

    // New file button
    const newFileBtn = document.createElement('span');
    newFileBtn.className = 'file-tree-collapse-btn';
    newFileBtn.textContent = '+';
    newFileBtn.title = 'New file';
    newFileBtn.addEventListener('click', () => {
      const root = store.get<string>('workspaceRoot');
      if (root) this.promptNewFile(root);
    });
    headerRight.appendChild(newFileBtn);

    // New folder button
    const newDirBtn = document.createElement('span');
    newDirBtn.className = 'file-tree-collapse-btn';
    newDirBtn.textContent = '⊞';
    newDirBtn.title = 'New folder';
    newDirBtn.addEventListener('click', () => {
      const root = store.get<string>('workspaceRoot');
      if (root) this.promptNewDir(root);
    });
    headerRight.appendChild(newDirBtn);

    const collapseBtn = document.createElement('span');
    collapseBtn.className = 'file-tree-collapse-btn';
    collapseBtn.textContent = '⌄';
    collapseBtn.title = 'Collapse all';
    collapseBtn.addEventListener('click', () => {
      this.expandedDirs.clear();
      this.refresh();
    });
    headerRight.appendChild(collapseBtn);

    const minimizeBtn = document.createElement('span');
    minimizeBtn.className = 'file-tree-collapse-btn';
    minimizeBtn.textContent = '−';
    minimizeBtn.title = 'Minimize sidebar (⌘B)';
    minimizeBtn.addEventListener('click', () => {
      store.set('fileTreeVisible', false);
    });
    headerRight.appendChild(minimizeBtn);

    header.appendChild(headerRight);

    this.container.appendChild(header);

    // Scrollable tree
    this.treeContainer = document.createElement('div');
    this.treeContainer.className = 'file-tree-content';
    this.container.appendChild(this.treeContainer);

    parent.appendChild(this.container);

    // Toggle visibility
    store.on('fileTreeVisible', (visible: boolean) => {
      this.container.classList.toggle('visible', visible);
    });

    // Listen for file changes to auto-refresh. Debounced: a build/run churns
    // through many .o/.gcda/.gcno files, each firing a change event — without
    // coalescing this rebuilds the whole tree (innerHTML) dozens of times.
    forge.workspace.onFileChanged(() => {
      this.scheduleRefresh();
    });

    // Watch active file for highlighting
    store.on('activeFilePath', () => {
      this.updateActiveHighlight();
    });

    // Dismiss context menu on click outside
    document.addEventListener('click', () => this.dismissContextMenu());

    // Drop on empty area = move to workspace root
    this.treeContainer.addEventListener('dragover', (e) => {
      if ((e.target as HTMLElement) === this.treeContainer) {
        e.preventDefault();
        e.dataTransfer!.dropEffect = 'move';
      }
    });
    this.treeContainer.addEventListener('drop', async (e) => {
      if ((e.target as HTMLElement) !== this.treeContainer) return;
      e.preventDefault();
      this.clearDropTargets();
      const sourcePath = e.dataTransfer!.getData('text/plain');
      if (!sourcePath) return;
      const root = store.get<string>('workspaceRoot');
      if (!root) return;
      const fileName = sourcePath.substring(sourcePath.lastIndexOf('/') + 1);
      const newPath = root + '/' + fileName;
      if (newPath === sourcePath) return;
      try {
        await forge.workspace.rename(sourcePath, newPath);
        this.refresh();
      } catch (err: any) {
        console.error('Move failed:', err);
      }
    });
  }

  private scheduleRefresh(): void {
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = null;
      this.refresh();
    }, 250);
  }

  async refresh(): Promise<void> {
    const tree = await forge.workspace.getTree();
    this.treeContainer.innerHTML = '';
    this.renderNodes(tree, this.treeContainer, 0);
  }

  private renderNodes(nodes: FileNode[], parent: HTMLElement, depth: number): void {
    for (const node of nodes) {
      const row = document.createElement('div');
      row.className = 'file-tree-row';
      row.style.paddingLeft = `${12 + depth * 16}px`;
      row.dataset.path = node.path;
      row.dataset.isDir = node.isDirectory ? '1' : '';

      // ── Drag source ──
      row.draggable = true;
      row.addEventListener('dragstart', (e) => {
        e.dataTransfer!.setData('text/plain', node.path);
        e.dataTransfer!.effectAllowed = 'move';
        row.classList.add('dragging');
      });
      row.addEventListener('dragend', () => {
        row.classList.remove('dragging');
        this.clearDropTargets();
      });

      // ── Drop target (directories only, but we handle drop on files by using their parent dir) ──
      row.addEventListener('dragover', (e) => {
        e.preventDefault();
        e.dataTransfer!.dropEffect = 'move';
        this.clearDropTargets();
        if (node.isDirectory) {
          row.classList.add('drop-target');
        } else {
          // Highlight parent directory row
          const parentDir = node.path.substring(0, node.path.lastIndexOf('/'));
          const parentRow = this.treeContainer.querySelector(`[data-path="${CSS.escape(parentDir)}"]`) as HTMLElement;
          if (parentRow) parentRow.classList.add('drop-target');
        }
      });
      row.addEventListener('dragleave', () => {
        row.classList.remove('drop-target');
      });
      row.addEventListener('drop', async (e) => {
        e.preventDefault();
        this.clearDropTargets();
        const sourcePath = e.dataTransfer!.getData('text/plain');
        if (!sourcePath || sourcePath === node.path) return;

        // Determine target directory
        let targetDir = node.isDirectory ? node.path : node.path.substring(0, node.path.lastIndexOf('/'));
        const fileName = sourcePath.substring(sourcePath.lastIndexOf('/') + 1);
        const newPath = targetDir + '/' + fileName;

        // Don't move into self or same location
        if (newPath === sourcePath) return;
        if (newPath.startsWith(sourcePath + '/')) return; // can't move folder into itself

        try {
          await forge.workspace.rename(sourcePath, newPath);
          if (node.isDirectory) this.expandedDirs.add(node.path);
          this.refresh();
        } catch (err: any) {
          console.error('Move failed:', err);
        }
      });

      const activePath = store.get<string>('activeFilePath');
      if (node.path === activePath) {
        row.classList.add('active');
      }

      if (node.isDirectory) {
        const expanded = this.expandedDirs.has(node.path);

        const arrow = document.createElement('span');
        arrow.className = `file-tree-arrow ${expanded ? 'expanded' : ''}`;
        arrow.textContent = '›';
        row.appendChild(arrow);

        const icon = document.createElement('span');
        icon.className = 'file-tree-icon dir';
        icon.textContent = expanded ? '▾' : '▸';
        row.appendChild(icon);

        const name = document.createElement('span');
        name.className = 'file-tree-name';
        name.textContent = node.name;
        row.appendChild(name);

        row.addEventListener('click', async () => {
          if (expanded) {
            this.expandedDirs.delete(node.path);
          } else {
            this.expandedDirs.add(node.path);
            if (!node.children || node.children.length === 0) {
              node.children = await forge.workspace.getTree(node.path);
            }
          }
          this.refresh();
        });

        // Right-click context menu
        row.addEventListener('contextmenu', (e) => {
          e.preventDefault();
          e.stopPropagation();
          this.showContextMenu(e.clientX, e.clientY, node, depth);
        });

        parent.appendChild(row);

        if (expanded && node.children) {
          this.renderNodes(node.children, parent, depth + 1);
        }
      } else {
        const spacer = document.createElement('span');
        spacer.className = 'file-tree-arrow-spacer';
        row.appendChild(spacer);

        const icon = document.createElement('span');
        icon.className = `file-tree-icon file ${this.getFileIconClass(node.name)}`;
        icon.textContent = this.getFileIcon(node.name);
        row.appendChild(icon);

        const name = document.createElement('span');
        name.className = 'file-tree-name';
        name.textContent = node.name;
        row.appendChild(name);

        row.addEventListener('click', () => {
          this.onFileOpen(node.path);
        });

        // Right-click context menu
        row.addEventListener('contextmenu', (e) => {
          e.preventDefault();
          e.stopPropagation();
          this.showContextMenu(e.clientX, e.clientY, node, depth);
        });

        parent.appendChild(row);
      }
    }
  }

  // ── Context menu ──

  private showContextMenu(x: number, y: number, node: FileNode, depth: number): void {
    this.dismissContextMenu();

    const menu = document.createElement('div');
    menu.className = 'file-tree-context-menu';
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;

    const items: { label: string; action: () => void }[] = [];

    if (node.isDirectory) {
      items.push({ label: 'New File', action: () => this.promptNewFile(node.path) });
      items.push({ label: 'New Folder', action: () => this.promptNewDir(node.path) });
    }

    items.push({ label: 'Rename', action: () => this.startRename(node) });
    items.push({ label: 'Delete', action: () => this.confirmDelete(node) });

    for (const item of items) {
      const el = document.createElement('div');
      el.className = 'file-tree-context-item';
      el.textContent = item.label;
      el.addEventListener('click', (e) => {
        e.stopPropagation();
        this.dismissContextMenu();
        item.action();
      });
      menu.appendChild(el);
    }

    document.body.appendChild(menu);
    this.contextMenu = menu;

    // Reposition if overflowing
    requestAnimationFrame(() => {
      const rect = menu.getBoundingClientRect();
      if (rect.right > window.innerWidth) menu.style.left = `${window.innerWidth - rect.width - 4}px`;
      if (rect.bottom > window.innerHeight) menu.style.top = `${window.innerHeight - rect.height - 4}px`;
    });
  }

  private clearDropTargets(): void {
    this.treeContainer.querySelectorAll('.drop-target').forEach(el => el.classList.remove('drop-target'));
  }

  private dismissContextMenu(): void {
    if (this.contextMenu) {
      this.contextMenu.remove();
      this.contextMenu = null;
    }
  }

  // ── Inline rename ──

  private startRename(node: FileNode): void {
    const row = this.treeContainer.querySelector(`[data-path="${CSS.escape(node.path)}"]`) as HTMLElement;
    if (!row) return;

    const nameEl = row.querySelector('.file-tree-name') as HTMLElement;
    if (!nameEl) return;

    const input = document.createElement('input');
    input.className = 'file-tree-rename-input';
    input.value = node.name;
    input.spellcheck = false;

    nameEl.replaceWith(input);
    input.focus();
    // Select name without extension for files
    if (!node.isDirectory) {
      const dotIdx = node.name.lastIndexOf('.');
      input.setSelectionRange(0, dotIdx > 0 ? dotIdx : node.name.length);
    } else {
      input.select();
    }

    const commit = async () => {
      const newName = input.value.trim();
      if (!newName || newName === node.name) {
        this.refresh();
        return;
      }
      const parentDir = node.path.substring(0, node.path.length - node.name.length);
      const newPath = parentDir + newName;
      try {
        await forge.workspace.rename(node.path, newPath);
      } catch (err: any) {
        console.error('Rename failed:', err);
      }
      this.refresh();
    };

    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); commit(); }
      if (e.key === 'Escape') { e.preventDefault(); this.refresh(); }
    });
    input.addEventListener('blur', commit);
  }

  // ── New file / folder ──

  private promptNewFile(parentDir: string): void {
    this.expandedDirs.add(parentDir);
    this.refresh().then(() => {
      this.showInlineInput(parentDir, false);
    });
  }

  private promptNewDir(parentDir: string): void {
    this.expandedDirs.add(parentDir);
    this.refresh().then(() => {
      this.showInlineInput(parentDir, true);
    });
  }

  private showInlineInput(parentDir: string, isDir: boolean): void {
    // Find the parent row to determine depth
    const parentRow = this.treeContainer.querySelector(`[data-path="${CSS.escape(parentDir)}"]`) as HTMLElement;
    const parentDepth = parentRow ? parseInt(parentRow.style.paddingLeft) : 12;
    const childPadding = parentDepth + 16;

    // Find insertion point — after parent row, before next sibling at same or lower depth
    let insertBefore: Element | null = null;
    if (parentRow) {
      let sibling = parentRow.nextElementSibling;
      while (sibling) {
        const sibPad = parseInt((sibling as HTMLElement).style.paddingLeft);
        if (sibPad <= parentDepth) { insertBefore = sibling; break; }
        sibling = sibling.nextElementSibling;
      }
    }

    const row = document.createElement('div');
    row.className = 'file-tree-row';
    row.style.paddingLeft = `${childPadding}px`;

    const spacer = document.createElement('span');
    spacer.className = 'file-tree-arrow-spacer';
    row.appendChild(spacer);

    const icon = document.createElement('span');
    icon.className = 'file-tree-icon';
    icon.textContent = isDir ? '▸' : '·';
    row.appendChild(icon);

    const input = document.createElement('input');
    input.className = 'file-tree-rename-input';
    input.placeholder = isDir ? 'folder name' : 'filename';
    input.spellcheck = false;
    row.appendChild(input);

    if (insertBefore) {
      this.treeContainer.insertBefore(row, insertBefore);
    } else {
      this.treeContainer.appendChild(row);
    }

    input.focus();

    const commit = async () => {
      const name = input.value.trim();
      row.remove();
      if (!name) return;

      const fullPath = parentDir + '/' + name;
      try {
        if (isDir) {
          await forge.workspace.createDir(fullPath);
        } else {
          await forge.workspace.createFile(fullPath);
          this.onFileOpen(fullPath);
        }
      } catch (err: any) {
        console.error('Create failed:', err);
      }
      this.refresh();
    };

    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); commit(); }
      if (e.key === 'Escape') { e.preventDefault(); row.remove(); }
    });
    input.addEventListener('blur', commit);
  }

  // ── Delete ──

  private async confirmDelete(node: FileNode): Promise<void> {
    const kind = node.isDirectory ? 'folder' : 'file';
    // Simple confirm — no native dialog needed in renderer
    const confirmed = window.confirm(`Delete ${kind} "${node.name}"?`);
    if (!confirmed) return;

    try {
      await forge.workspace.delete(node.path);
    } catch (err: any) {
      console.error('Delete failed:', err);
    }
    this.refresh();
  }

  // ── Highlight ──

  private updateActiveHighlight(): void {
    const activePath = store.get<string>('activeFilePath');
    const rows = this.treeContainer.querySelectorAll('.file-tree-row');
    for (const row of rows) {
      const el = row as HTMLElement;
      el.classList.toggle('active', el.dataset.path === activePath);
    }
  }

  // ── Icons ──

  private getFileIcon(name: string): string {
    const ext = name.split('.').pop()?.toLowerCase() || '';
    const icons: Record<string, string> = {
      'cpp': '◆', 'cc': '◆', 'cxx': '◆', 'c': '◇',
      'h': '◈', 'hpp': '◈', 'hxx': '◈',
      'json': '{}', 'txt': '≡', 'md': '¶',
      'py': '◎', 'sh': '$', 'zsh': '$',
      'makefile': '⚙', 'cmake': '⚙',
      'cu': '▣', 'metal': '▣',
    };
    return icons[ext] || '·';
  }

  private getFileIconClass(name: string): string {
    const ext = name.split('.').pop()?.toLowerCase() || '';
    if (['cpp', 'cc', 'cxx', 'c', 'cu', 'metal'].includes(ext)) return 'source';
    if (['h', 'hpp', 'hxx'].includes(ext)) return 'header';
    return '';
  }
}
