import * as monaco from 'monaco-editor';

export const FORGE_THEME_NAME = 'forge-dark';
export const FORGE_LIGHT_THEME_NAME = 'forge-light';

export function registerForgeTheme(): void {
  monaco.editor.defineTheme(FORGE_THEME_NAME, {
    base: 'vs-dark',
    inherit: false,
    rules: [
      // Keywords: if, for, return, class, struct, etc.
      { token: 'keyword', foreground: '56B389', fontStyle: '' },
      { token: 'keyword.control', foreground: '56B389' },
      { token: 'keyword.operator', foreground: 'A0B0A8' },

      // Types and classes
      { token: 'type', foreground: '9BB5CF' },
      { token: 'type.identifier', foreground: '9BB5CF' },
      { token: 'class', foreground: '9BB5CF' },
      { token: 'struct', foreground: '9BB5CF' },
      { token: 'enum', foreground: '9BB5CF' },

      // Functions and methods
      { token: 'function', foreground: '8DB2FF' },
      { token: 'function.declaration', foreground: '8DB2FF' },
      { token: 'method', foreground: '8DB2FF' },

      // Variables and identifiers
      { token: 'variable', foreground: 'E2E8E4' },
      { token: 'identifier', foreground: 'E2E8E4' },
      { token: 'parameter', foreground: 'E2E8E4' },

      // Constants and enums
      { token: 'constant', foreground: '8DB2FF' },
      { token: 'enumMember', foreground: '8DB2FF' },

      // Strings
      { token: 'string', foreground: 'D4A76A' },
      { token: 'string.escape', foreground: 'CF9C6B' },

      // Numbers
      { token: 'number', foreground: 'CF9C6B' },
      { token: 'number.hex', foreground: 'CF9C6B' },
      { token: 'number.float', foreground: 'CF9C6B' },

      // Comments
      { token: 'comment', foreground: '6B7A72', fontStyle: '' },
      { token: 'comment.doc', foreground: '6B7A72' },

      // Preprocessor
      { token: 'annotation', foreground: '7A9484' },
      { token: 'tag', foreground: '7A9484' },
      { token: 'metatag', foreground: '7A9484' },
      { token: 'macro', foreground: '7A9484' },

      // Operators and punctuation
      { token: 'operator', foreground: 'A0B0A8' },
      { token: 'delimiter', foreground: '8A9A90' },
      { token: 'delimiter.bracket', foreground: '8A9A90' },
      { token: 'delimiter.parenthesis', foreground: '8A9A90' },
      { token: 'delimiter.curly', foreground: '8A9A90' },
      { token: 'delimiter.square', foreground: '8A9A90' },
      { token: 'delimiter.angle', foreground: '8A9A90' },

      // Namespace
      { token: 'namespace', foreground: '9BB5CF' },

      // Metal — attribute annotations in amber
      { token: 'annotation', foreground: 'D4A76A' },

      // Assembly — instructions in blue, function labels bold teal,
      // branch labels dim, registers in red, SIMD in green
      { token: 'entity.name.function', foreground: '8DB2FF' },
      { token: 'variable.name', foreground: 'CF6B6B' },
      { token: 'type.identifier', foreground: '56B389', fontStyle: 'bold' },

      // Default
      { token: '', foreground: 'E2E8E4' },
    ],
    colors: {
      // Editor — JetBrains "New UI" charcoal. Editor surface (#1E1F22) is the
      // darkest, one step below the lifted panels (#2B2D30); text is a calm
      // neutral off-white for lower contrast. Selection, cursor, and match
      // highlights use the emerald accent (--chart-1).
      'editor.background': '#1E1F22',
      'editor.foreground': '#DFE1E5',
      'editor.lineHighlightBackground': '#26282E',
      'editor.lineHighlightBorder': '#00000000',
      'editor.selectionBackground': '#56B38926',
      'editor.selectionHighlightBackground': '#56B38915',
      'editor.inactiveSelectionBackground': '#56B38912',
      'editor.findMatchBackground': '#56B38930',
      'editor.findMatchHighlightBackground': '#56B38918',
      'editor.wordHighlightBackground': '#56B38915',
      'editor.wordHighlightStrongBackground': '#56B38922',

      // Cursor
      'editorCursor.foreground': '#56B389',
      'editorWhitespace.foreground': '#ffffff0a',

      // Line numbers
      'editorLineNumber.foreground': '#4B4E56',
      'editorLineNumber.activeForeground': '#868A91',

      // Gutter
      'editorGutter.background': '#1E1F22',
      'editorGutter.addedBackground': '#56B389',
      'editorGutter.modifiedBackground': '#8DB2FF',
      'editorGutter.deletedBackground': '#CF6B6B',

      // Indent guides
      'editorIndentGuide.background': '#ffffff08',
      'editorIndentGuide.activeBackground': '#ffffff15',

      // Bracket matching
      'editorBracketMatch.background': '#56B38920',
      'editorBracketMatch.border': '#56B38940',

      // Scrollbar
      'scrollbar.shadow': '#00000000',
      'scrollbarSlider.background': '#ffffff14',
      'scrollbarSlider.hoverBackground': '#ffffff26',
      'scrollbarSlider.activeBackground': '#ffffff33',

      // Minimap (disabled, but just in case)
      'minimap.background': '#1E1F22',

      // Widget (autocomplete, hover) — lifted popover surface, soft border
      'editorWidget.background': '#2B2D30',
      'editorWidget.border': '#393B40',
      'editorWidget.foreground': '#DFE1E5',
      'editorSuggestWidget.background': '#2B2D30',
      'editorSuggestWidget.border': '#393B40',
      'editorSuggestWidget.selectedBackground': '#393B40',
      'editorSuggestWidget.highlightForeground': '#56B389',
      'editorHoverWidget.background': '#2B2D30',
      'editorHoverWidget.border': '#393B40',

      // Errors and warnings
      'editorError.foreground': '#CF6B6B',
      'editorWarning.foreground': '#D4A76A',
      'editorInfo.foreground': '#8DB2FF',

      // Overview ruler (scrollbar markers)
      'editorOverviewRuler.errorForeground': '#CF6B6B',
      'editorOverviewRuler.warningForeground': '#D4A76A',
      'editorOverviewRuler.infoForeground': '#8DB2FF',
      'editorOverviewRuler.findMatchForeground': '#56B38960',
      'editorOverviewRuler.selectionHighlightForeground': '#56B38940',
      'editorOverviewRuler.modifiedForeground': '#8DB2FF80',
      'editorOverviewRuler.addedForeground': '#56B38980',
      'editorOverviewRuler.deletedForeground': '#CF6B6B80',
      'editorOverviewRuler.bracketMatchForeground': '#56B38940',
      'editorOverviewRuler.border': '#00000000',
      'editorOverviewRuler.background': '#1E1F22',

      // Overscroll
      'editor.rangeHighlightBackground': '#56B38910',

      // Input (command palette, quick open)
      'input.background': '#1E1F22',
      'input.foreground': '#DFE1E5',
      'input.border': '#393B40',
      'input.placeholderForeground': '#868A91',
      'inputOption.activeBorder': '#56B389',

      // List (autocomplete, file tree)
      'list.hoverBackground': '#2E3033',
      'list.activeSelectionBackground': '#393B40',
      'list.activeSelectionForeground': '#DFE1E5',
      'list.inactiveSelectionBackground': '#2E3033',
      'list.focusBackground': '#393B40',
      'list.highlightForeground': '#56B389',

      // Diff
      'diffEditor.insertedTextBackground': '#56B38918',
      'diffEditor.removedTextBackground': '#CF6B6B18',
    },
  });

  registerForgeLightTheme();
}

/**
 * Light theme designed to minimize eye strain based on visual-ergonomics research:
 *   • Warm cream background (#F4EFE2) instead of pure white — reduces blue-light
 *     exposure and contrast shock. Pure white backgrounds (#FFFFFF) trigger more
 *     photoreceptor fatigue than a slightly tinted off-white around 90% luminance.
 *   • Dark warm charcoal text (#373528) instead of pure black — studies (Bauer et al.,
 *     Journal of Vision) show that contrast ratios around 10:1 are more comfortable
 *     than the 21:1 of pure #000 on #FFF for extended reading/editing.
 *   • Desaturated accent colors in the green/amber family — green is the least
 *     fatiguing hue for the eye (peak photopic sensitivity at ~555nm) which is
 *     why accountants and oscilloscopes historically used it. Amber/warm tones
 *     pair well at lower color temperatures and don't spike cortisol like cool blue.
 *   • Soft borders (black @ 8% alpha) avoid the "grid of doom" effect.
 */
function registerForgeLightTheme(): void {
  monaco.editor.defineTheme(FORGE_LIGHT_THEME_NAME, {
    base: 'vs',
    inherit: false,
    rules: [
      { token: 'keyword', foreground: '4A7A5C', fontStyle: '' },
      { token: 'keyword.control', foreground: '4A7A5C' },
      { token: 'keyword.operator', foreground: '6D6A58' },
      { token: 'type', foreground: '3E6F9E' },
      { token: 'type.identifier', foreground: '3E6F9E' },
      { token: 'class', foreground: '3E6F9E' },
      { token: 'struct', foreground: '3E6F9E' },
      { token: 'enum', foreground: '3E6F9E' },
      { token: 'function', foreground: '3E6F9E' },
      { token: 'function.declaration', foreground: '3E6F9E' },
      { token: 'method', foreground: '3E6F9E' },
      { token: 'variable', foreground: '373528' },
      { token: 'identifier', foreground: '373528' },
      { token: 'parameter', foreground: '373528' },
      { token: 'constant', foreground: '3E6F9E' },
      { token: 'enumMember', foreground: '3E6F9E' },
      { token: 'string', foreground: '9A6A2A' },
      { token: 'string.escape', foreground: 'AA7232' },
      { token: 'number', foreground: 'AA7232' },
      { token: 'number.hex', foreground: 'AA7232' },
      { token: 'number.float', foreground: 'AA7232' },
      { token: 'comment', foreground: '8A8574', fontStyle: 'italic' },
      { token: 'comment.doc', foreground: '8A8574' },
      { token: 'annotation', foreground: '6A8A4C' },
      { token: 'operator', foreground: '6D6A58' },
      { token: 'delimiter', foreground: '7A7662' },
      { token: 'delimiter.bracket', foreground: '7A7662' },
      { token: 'delimiter.parenthesis', foreground: '7A7662' },
      { token: 'delimiter.curly', foreground: '7A7662' },
      { token: 'delimiter.square', foreground: '7A7662' },
      { token: 'delimiter.angle', foreground: '7A7662' },
      { token: 'namespace', foreground: '3E6F9E' },

      // Metal attribute annotations
      { token: 'annotation', foreground: 'A06C2C' },

      // Assembly
      { token: 'entity.name.function', foreground: '3E6F9E' },
      { token: 'variable.name', foreground: 'A54545' },
      { token: 'type.identifier', foreground: '4A7A5C', fontStyle: 'bold' },

      { token: '', foreground: '373528' },
    ],
    colors: {
      'editor.background': '#F4EFE2',
      'editor.foreground': '#373528',
      'editor.lineHighlightBackground': '#EDE6D4',
      'editor.lineHighlightBorder': '#00000000',
      'editor.selectionBackground': '#4A7A5C30',
      'editor.selectionHighlightBackground': '#4A7A5C18',
      'editor.inactiveSelectionBackground': '#4A7A5C15',
      'editor.findMatchBackground': '#4A7A5C40',
      'editor.findMatchHighlightBackground': '#4A7A5C20',
      'editor.wordHighlightBackground': '#4A7A5C18',
      'editor.wordHighlightStrongBackground': '#4A7A5C25',

      'editorCursor.foreground': '#4A7A5C',
      'editorWhitespace.foreground': '#00000012',

      'editorLineNumber.foreground': '#8A857470',
      'editorLineNumber.activeForeground': '#6D6A58',

      'editorGutter.background': '#F4EFE2',
      'editorGutter.addedBackground': '#4A7A5C',
      'editorGutter.modifiedBackground': '#3E6F9E',
      'editorGutter.deletedBackground': '#A54545',

      'editorIndentGuide.background': '#00000010',
      'editorIndentGuide.activeBackground': '#00000020',

      'editorBracketMatch.background': '#4A7A5C25',
      'editorBracketMatch.border': '#4A7A5C50',

      'scrollbar.shadow': '#00000000',
      'scrollbarSlider.background': '#00000015',
      'scrollbarSlider.hoverBackground': '#00000025',
      'scrollbarSlider.activeBackground': '#00000035',

      'minimap.background': '#F4EFE2',

      'editorWidget.background': '#EAE4D5',
      'editorWidget.border': '#00000012',
      'editorWidget.foreground': '#373528',
      'editorSuggestWidget.background': '#EAE4D5',
      'editorSuggestWidget.border': '#00000012',
      'editorSuggestWidget.selectedBackground': '#DED7C6',
      'editorSuggestWidget.highlightForeground': '#4A7A5C',
      'editorHoverWidget.background': '#EAE4D5',
      'editorHoverWidget.border': '#00000012',

      'editorError.foreground': '#A54545',
      'editorWarning.foreground': '#A06C2C',
      'editorInfo.foreground': '#3E6F9E',

      'editorOverviewRuler.errorForeground': '#A54545',
      'editorOverviewRuler.warningForeground': '#A06C2C',
      'editorOverviewRuler.infoForeground': '#3E6F9E',
      'editorOverviewRuler.findMatchForeground': '#4A7A5C60',
      'editorOverviewRuler.selectionHighlightForeground': '#4A7A5C40',
      'editorOverviewRuler.border': '#00000000',
      'editorOverviewRuler.background': '#F4EFE2',

      'editor.rangeHighlightBackground': '#4A7A5C10',

      'input.background': '#EAE4D5',
      'input.foreground': '#373528',
      'input.border': '#00000012',
      'input.placeholderForeground': '#8A8574',
      'inputOption.activeBorder': '#4A7A5C',

      'list.hoverBackground': '#E5DFCE',
      'list.activeSelectionBackground': '#DED7C6',
      'list.activeSelectionForeground': '#373528',
      'list.inactiveSelectionBackground': '#E5DFCE',
      'list.focusBackground': '#DED7C6',
      'list.highlightForeground': '#4A7A5C',

      'diffEditor.insertedTextBackground': '#4A7A5C18',
      'diffEditor.removedTextBackground': '#A5454518',
    },
  });
}
