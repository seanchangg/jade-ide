import * as monaco from 'monaco-editor';
import { store } from '../state';

// AI inline completion — Copilot-style ghost text backed by a local
// fill-in-the-middle model (see src/main/inline-completion.ts).
//
// Lessons borrowed from JetBrains' Full Line Code Completion (arXiv 2405.08704):
//  - debounce and cancel aggressively; a stale suggestion is worse than none
//  - cache raw model output so typing through a suggestion never re-queries
//  - post-process line-aware: cut at blank lines, cap length, never duplicate
//    what already follows the cursor

const forge = (window as any).forge;

const LANGUAGES = ['cpp', 'c', 'metal', 'objective-c', 'python', 'javascript', 'typescript', 'shell'];

const DEBOUNCE_MS = 120;
const MAX_PREFIX_CHARS = 6000;
const MAX_SUFFIX_CHARS = 2000;
const MAX_LINES = 6;
const CACHE_SIZE = 48;

interface CacheEntry {
  prefix: string;
  suffix: string;
  content: string; // raw model output, post-processed at serve time
}

const cache: CacheEntry[] = [];

function cachePut(prefix: string, suffix: string, content: string): void {
  cache.push({ prefix, suffix, content });
  if (cache.length > CACHE_SIZE) cache.shift();
}

// Exact hit, or a "typed-through" hit: the user typed characters that match
// the start of an earlier suggestion, so serve the remainder instantly.
function cacheLookup(prefix: string, suffix: string): string | null {
  for (let i = cache.length - 1; i >= 0; i--) {
    const e = cache[i];
    if (e.suffix !== suffix) continue;
    if (e.prefix === prefix) return e.content;
    if (prefix.startsWith(e.prefix)) {
      const typed = prefix.slice(e.prefix.length);
      if (typed.length < e.content.length && e.content.startsWith(typed)) {
        return e.content.slice(typed.length);
      }
    }
  }
  return null;
}

function postProcess(raw: string, lineSuffix: string, maxLines: number): string | null {
  let text = raw.replace(/\r/g, '');

  // Stop at the first blank line — beyond it the model is usually rambling
  const blank = text.search(/\n[ \t]*\n/);
  if (blank >= 0) text = text.slice(0, blank);

  const lines = text.split('\n');
  if (lines.length > maxLines) text = lines.slice(0, maxLines).join('\n');

  text = text.replace(/\s+$/, '');

  // Don't duplicate what already sits after the cursor on this line
  // (e.g. an auto-inserted closing paren the model also generated)
  const tail = lineSuffix.trim();
  if (tail && text.endsWith(tail)) {
    text = text.slice(0, text.length - tail.length).replace(/\s+$/, '');
  }

  return text.trim() ? text : null;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

export function registerInlineCompletion(): void {
  // Cached output was generated for one mode — drop it when the mode flips
  store.on('aiMultiline', () => { cache.length = 0; });

  const provider: monaco.languages.InlineCompletionsProvider = {
    async provideInlineCompletions(model, position, _context, token) {
      const empty = { items: [] as monaco.languages.InlineCompletion[] };

      if (store.get<boolean>('aiCompletionEnabled') === false) return empty;
      if (store.get<string>('aiStatus') !== 'ready') return empty;

      const multiline = store.get<boolean>('aiMultiline') === true;
      const maxLines = multiline ? MAX_LINES : 1;

      const lastLine = model.getLineCount();
      const fullPrefix = model.getValueInRange(
        new monaco.Range(1, 1, position.lineNumber, position.column)
      );
      const fullSuffix = model.getValueInRange(
        new monaco.Range(position.lineNumber, position.column, lastLine, model.getLineMaxColumn(lastLine))
      );
      const prefix = fullPrefix.slice(-MAX_PREFIX_CHARS);
      const suffix = fullSuffix.slice(0, MAX_SUFFIX_CHARS);
      const lineSuffix = model.getLineContent(position.lineNumber).slice(position.column - 1);

      const makeResult = (text: string): monaco.languages.InlineCompletions => ({
        items: [{
          insertText: text,
          range: new monaco.Range(
            position.lineNumber, position.column,
            position.lineNumber, position.column
          ),
        }],
        enableForwardStability: true,
      });

      const cached = cacheLookup(prefix, suffix);
      if (cached !== null) {
        const text = postProcess(cached, lineSuffix, maxLines);
        return text ? makeResult(text) : empty;
      }

      // Debounce: Monaco cancels the token when the user keeps typing
      await sleep(DEBOUNCE_MS);
      if (token.isCancellationRequested) return empty;

      const result = await forge.ai.infill({
        prefix, suffix,
        filename: model.uri.path,
        singleLine: !multiline,
      });
      if (!result || !result.content || token.isCancellationRequested) return empty;

      cachePut(prefix, suffix, result.content);
      const text = postProcess(result.content, lineSuffix, maxLines);
      return text ? makeResult(text) : empty;
    },

    freeInlineCompletions() {},
  };

  for (const lang of LANGUAGES) {
    monaco.languages.registerInlineCompletionsProvider(lang, provider);
  }
}
