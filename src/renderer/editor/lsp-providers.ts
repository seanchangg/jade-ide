import * as monaco from 'monaco-editor';
import { store } from '../state';

const forge = (window as any).forge;

// Register Monaco language providers that forward to clangd via IPC.
// Each provider is async — Monaco handles the loading UI.

export function registerLspProviders(): void {
  const selector = [
    { language: 'cpp', scheme: 'file' },
    { language: 'c', scheme: 'file' },
  ];

  // ── Completion ──
  monaco.languages.registerCompletionItemProvider('cpp', {
    triggerCharacters: ['.', ':', '>', '<', '"', '/'],

    async provideCompletionItems(model, position) {
      if (!store.get<boolean>('lspReady')) return { suggestions: [] };

      const uri = model.uri.toString();
      const result = await forge.lsp.completion(uri, position.lineNumber - 1, position.column - 1);

      if (!result || !result.items) return { suggestions: [] };

      const word = model.getWordUntilPosition(position);
      const range = new monaco.Range(
        position.lineNumber, word.startColumn,
        position.lineNumber, word.endColumn
      );

      const suggestions: monaco.languages.CompletionItem[] = result.items.map((item: any) => ({
        label: item.label || item.filterText || '',
        kind: mapCompletionKind(item.kind),
        detail: item.detail || '',
        documentation: item.documentation
          ? { value: typeof item.documentation === 'string' ? item.documentation : item.documentation.value }
          : undefined,
        insertText: item.textEdit?.newText || item.insertText || item.label || '',
        insertTextRules: item.insertTextFormat === 2
          ? monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet
          : undefined,
        range,
        sortText: item.sortText || item.label,
      }));

      return { suggestions };
    },
  });

  // ── Hover ──
  monaco.languages.registerHoverProvider('cpp', {
    async provideHover(model, position) {
      if (!store.get<boolean>('lspReady')) return null;

      const uri = model.uri.toString();
      const result = await forge.lsp.hover(uri, position.lineNumber - 1, position.column - 1);

      if (!result || !result.contents) return null;

      const contents: monaco.IMarkdownString[] = [];
      if (typeof result.contents === 'string') {
        contents.push({ value: result.contents });
      } else if (result.contents.value) {
        contents.push({ value: result.contents.value });
      } else if (Array.isArray(result.contents)) {
        for (const c of result.contents) {
          contents.push({ value: typeof c === 'string' ? c : c.value || '' });
        }
      }

      return {
        contents,
        range: result.range ? new monaco.Range(
          result.range.start.line + 1, result.range.start.character + 1,
          result.range.end.line + 1, result.range.end.character + 1,
        ) : undefined,
      };
    },
  });

  // ── Go to definition ──
  monaco.languages.registerDefinitionProvider('cpp', {
    async provideDefinition(model, position) {
      if (!store.get<boolean>('lspReady')) return null;

      const uri = model.uri.toString();
      const locations = await forge.lsp.definition(uri, position.lineNumber - 1, position.column - 1);

      if (!locations || locations.length === 0) return null;

      return locations.map((loc: any) => ({
        uri: monaco.Uri.parse(loc.uri),
        range: new monaco.Range(
          loc.range.start.line + 1, loc.range.start.character + 1,
          loc.range.end.line + 1, loc.range.end.character + 1,
        ),
      }));
    },
  });

  // ── References ──
  monaco.languages.registerReferenceProvider('cpp', {
    async provideReferences(model, position) {
      if (!store.get<boolean>('lspReady')) return null;

      const uri = model.uri.toString();
      const locations = await forge.lsp.references(uri, position.lineNumber - 1, position.column - 1);

      if (!locations || locations.length === 0) return null;

      return locations.map((loc: any) => ({
        uri: monaco.Uri.parse(loc.uri),
        range: new monaco.Range(
          loc.range.start.line + 1, loc.range.start.character + 1,
          loc.range.end.line + 1, loc.range.end.character + 1,
        ),
      }));
    },
  });

  // ── Diagnostics (pushed from clangd) ──
  forge.lsp.onDiagnostics((params: any) => {
    const uri = monaco.Uri.parse(params.uri);
    const model = monaco.editor.getModel(uri);
    if (!model) return;

    // Skip diagnostics for Metal shader files — clangd doesn't understand MSL
    if (uri.path.endsWith('.metal')) {
      monaco.editor.setModelMarkers(model, 'clangd', []);
      return;
    }

    const markers: monaco.editor.IMarkerData[] = (params.diagnostics || []).map((d: any) => ({
      severity: mapDiagnosticSeverity(d.severity),
      message: d.message,
      startLineNumber: d.range.start.line + 1,
      startColumn: d.range.start.character + 1,
      endLineNumber: d.range.end.line + 1,
      endColumn: d.range.end.character + 1,
      source: d.source || 'clangd',
    }));

    monaco.editor.setModelMarkers(model, 'clangd', markers);
  });
}

function mapCompletionKind(kind: number): monaco.languages.CompletionItemKind {
  // LSP CompletionItemKind to Monaco CompletionItemKind
  const map: Record<number, monaco.languages.CompletionItemKind> = {
    1: monaco.languages.CompletionItemKind.Text,
    2: monaco.languages.CompletionItemKind.Method,
    3: monaco.languages.CompletionItemKind.Function,
    4: monaco.languages.CompletionItemKind.Constructor,
    5: monaco.languages.CompletionItemKind.Field,
    6: monaco.languages.CompletionItemKind.Variable,
    7: monaco.languages.CompletionItemKind.Class,
    8: monaco.languages.CompletionItemKind.Interface,
    9: monaco.languages.CompletionItemKind.Module,
    10: monaco.languages.CompletionItemKind.Property,
    11: monaco.languages.CompletionItemKind.Unit,
    12: monaco.languages.CompletionItemKind.Value,
    13: monaco.languages.CompletionItemKind.Enum,
    14: monaco.languages.CompletionItemKind.Keyword,
    15: monaco.languages.CompletionItemKind.Snippet,
    22: monaco.languages.CompletionItemKind.Struct,
    23: monaco.languages.CompletionItemKind.Event,
  };
  return map[kind] || monaco.languages.CompletionItemKind.Text;
}

function mapDiagnosticSeverity(severity: number): monaco.MarkerSeverity {
  switch (severity) {
    case 1: return monaco.MarkerSeverity.Error;
    case 2: return monaco.MarkerSeverity.Warning;
    case 3: return monaco.MarkerSeverity.Info;
    case 4: return monaco.MarkerSeverity.Hint;
    default: return monaco.MarkerSeverity.Info;
  }
}
