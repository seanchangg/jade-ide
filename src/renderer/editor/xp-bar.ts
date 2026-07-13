import * as monaco from 'monaco-editor';
import { store } from '../state';

const forge = (window as any).forge;

// ── Tuning ──
const STREAK_TIMEOUT_MS = 10_000;  // 10s without typing breaks the streak
const XP_PER_LINE_BASE = 1;         // base XP per new line
// XP needed to advance from level N to N+1 = LEVEL_BASE + (N-1) * LEVEL_STEP
// L1→L2: 150, L2→L3: 250, L3→L4: 350, ... each level needs 100 more than the last
const LEVEL_BASE = 150;
const LEVEL_STEP = 100;

function levelFromXp(totalXp: number): { level: number; progress: number; needed: number } {
  let level = 1;
  let needed = LEVEL_BASE;
  let remaining = totalXp;
  while (remaining >= needed) {
    remaining -= needed;
    level++;
    needed += LEVEL_STEP;
  }
  return { level, progress: remaining, needed };
}

let containerEl: HTMLElement | null = null;
let fillEl: HTMLElement | null = null;
let labelEl: HTMLElement | null = null;
let streakEl: HTMLElement | null = null;

let totalXp = 0;
let streak = 1;
let streakTimer: ReturnType<typeof setTimeout> | null = null;

export function mountXpBar(parent: HTMLElement): void {
  containerEl = document.createElement('div');
  containerEl.className = 'xp-bar';

  labelEl = document.createElement('span');
  labelEl.className = 'xp-label';
  containerEl.appendChild(labelEl);

  const track = document.createElement('div');
  track.className = 'xp-track';
  fillEl = document.createElement('div');
  fillEl.className = 'xp-fill';
  track.appendChild(fillEl);
  containerEl.appendChild(track);

  streakEl = document.createElement('span');
  streakEl.className = 'xp-streak';
  containerEl.appendChild(streakEl);

  parent.appendChild(containerEl);

  // Load persisted total
  forge.state.load('xpTotal').then((val: any) => {
    if (typeof val === 'number') totalXp = val;
    render();
  });
}

export function attachXpToEditor(editor: monaco.editor.IStandaloneCodeEditor): void {
  editor.onDidChangeModelContent((e) => {
    const model = editor.getModel();
    if (!model) return;

    // Count each change that inserted a newline AND where the completed line
    // ends with a semicolon. Plain Enters on empty/unfinished lines don't count.
    let credits = 0;
    for (const change of e.changes) {
      const inserted = change.text;
      if (!inserted || !inserted.includes('\n')) continue;
      // Skip huge pastes — not "coding"
      if (inserted.length > 300) continue;

      const ln = change.range.startLineNumber;
      if (ln < 1 || ln > model.getLineCount()) continue;
      const completedLine = model.getLineContent(ln).trim();
      // Strip trailing line comments so "foo(); // done" still counts
      const codePart = completedLine.replace(/\/\/.*$/, '').trim();
      if (codePart.endsWith(';')) {
        credits++;
      }
    }
    if (credits === 0) return;

    totalXp += XP_PER_LINE_BASE * streak * credits;

    // Extend or start streak
    if (streakTimer) {
      clearTimeout(streakTimer);
      streak++;
    }
    streakTimer = setTimeout(() => {
      streak = 1;
      streakTimer = null;
      render();
    }, STREAK_TIMEOUT_MS);

    render();
    persist();
  });
}

let persistTimer: ReturnType<typeof setTimeout> | null = null;
function persist(): void {
  if (persistTimer) return;
  persistTimer = setTimeout(() => {
    forge.state.save('xpTotal', totalXp);
    persistTimer = null;
  }, 2000);
}

function render(): void {
  if (!containerEl || !fillEl || !labelEl || !streakEl) return;
  const { level, progress, needed } = levelFromXp(totalXp);
  const pct = (progress / needed) * 100;

  labelEl.textContent = `L${level}`;
  labelEl.title = `${progress} / ${needed} XP to L${level + 1}  •  total ${totalXp}`;
  fillEl.style.width = `${pct}%`;

  if (streak > 1) {
    streakEl.textContent = `×${streak}`;
    streakEl.classList.add('active');
  } else {
    streakEl.textContent = '';
    streakEl.classList.remove('active');
  }
}
