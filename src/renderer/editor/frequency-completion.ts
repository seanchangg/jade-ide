import * as monaco from 'monaco-editor';

// Frequency-based word completion.
// Suggests words that appear 2+ times in the current buffer, ranked by frequency.
// Works alongside LSP (Monaco merges multiple completion providers).

const MIN_OCCURRENCES = 2;
const MIN_WORD_LENGTH = 3;

export function registerFrequencyCompletion(): void {
  const provider: monaco.languages.CompletionItemProvider = {
    provideCompletionItems(model, position) {
      const word = model.getWordUntilPosition(position);
      if (word.word.length < 1) return { suggestions: [] };

      const range = new monaco.Range(
        position.lineNumber, word.startColumn,
        position.lineNumber, word.endColumn
      );

      // Count all identifier-like words in the buffer
      const text = model.getValue();
      const re = /[A-Za-z_][A-Za-z0-9_]*/g;
      const counts = new Map<string, number>();
      let match;
      while ((match = re.exec(text)) !== null) {
        const w = match[0];
        if (w.length < MIN_WORD_LENGTH) continue;
        counts.set(w, (counts.get(w) || 0) + 1);
      }

      // Exclude the word currently being typed from its own suggestion
      const currentWord = word.word;
      const suggestions: monaco.languages.CompletionItem[] = [];

      for (const [w, count] of counts) {
        if (count < MIN_OCCURRENCES) continue;
        if (w === currentWord) continue;
        // Case-insensitive prefix match against what the user has typed
        if (currentWord && !w.toLowerCase().startsWith(currentWord.toLowerCase())) continue;

        suggestions.push({
          label: w,
          kind: monaco.languages.CompletionItemKind.Text,
          detail: `×${count}`,
          insertText: w,
          range,
          // Sort by frequency (higher count = earlier). Use inverse so more frequent sorts first alphabetically.
          sortText: String(10000 - count).padStart(5, '0') + w,
        });
      }

      return { suggestions };
    },
  };

  // Register for languages the user writes code in
  for (const lang of ['cpp', 'c', 'metal', 'python', 'javascript', 'typescript', 'objective-c']) {
    monaco.languages.registerCompletionItemProvider(lang, provider);
  }
}
