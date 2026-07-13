import * as monaco from 'monaco-editor';

export function registerMetalLanguage(): void {
  monaco.languages.register({ id: 'metal' });

  // Language configuration — brackets, comments, auto-closing pairs, indent rules
  monaco.languages.setLanguageConfiguration('metal', {
    comments: {
      lineComment: '//',
      blockComment: ['/*', '*/'],
    },
    brackets: [
      ['{', '}'],
      ['[', ']'],
      ['(', ')'],
    ],
    autoClosingPairs: [
      { open: '{', close: '}' },
      { open: '[', close: ']' },
      { open: '(', close: ')' },
      { open: '"', close: '"', notIn: ['string'] },
      { open: "'", close: "'", notIn: ['string', 'comment'] },
      { open: '/*', close: ' */', notIn: ['string'] },
    ],
    surroundingPairs: [
      { open: '{', close: '}' },
      { open: '[', close: ']' },
      { open: '(', close: ')' },
      { open: '"', close: '"' },
      { open: "'", close: "'" },
      { open: '<', close: '>' },
    ],
    indentationRules: {
      increaseIndentPattern: /^.*\{[^}"']*$/,
      decreaseIndentPattern: /^\s*\}/,
    },
    onEnterRules: [
      {
        // Continue block comments
        beforeText: /^\s*\/\*(?!.*\*\/\s*$).*$/,
        afterText: /^\s*\*\/\s*$/,
        action: { indentAction: monaco.languages.IndentAction.IndentOutdent, appendText: ' * ' },
      },
      {
        // Continue " * " inside block comments
        beforeText: /^\s*\/?\*(?!\/).*$/,
        action: { indentAction: monaco.languages.IndentAction.None, appendText: '* ' },
      },
    ],
    wordPattern: /[a-zA-Z_][a-zA-Z0-9_]*/,
  });

  monaco.languages.setMonarchTokensProvider('metal', {
    defaultToken: '',
    keywords: [
      // C++ keywords
      'auto', 'break', 'case', 'const', 'continue', 'default', 'do', 'else',
      'enum', 'extern', 'for', 'goto', 'if', 'inline', 'register', 'return',
      'sizeof', 'static', 'struct', 'switch', 'typedef', 'union', 'volatile',
      'while', 'class', 'namespace', 'template', 'typename', 'using',
      'true', 'false', 'nullptr', 'constexpr', 'static_cast', 'const_cast',
      // Metal-specific keywords
      'kernel', 'vertex', 'fragment', 'compute',
      'device', 'constant', 'threadgroup', 'threadgroup_imageblock',
      'thread', 'ray_data',
      'stage_in',
      'visible_function_table',
    ],

    typeKeywords: [
      // Metal types
      'half', 'half2', 'half3', 'half4',
      'float', 'float2', 'float3', 'float4',
      'float2x2', 'float3x3', 'float4x4',
      'half2x2', 'half3x3', 'half4x4',
      'int', 'int2', 'int3', 'int4',
      'uint', 'uint2', 'uint3', 'uint4',
      'short', 'short2', 'short3', 'short4',
      'ushort', 'ushort2', 'ushort3', 'ushort4',
      'char', 'uchar', 'bool',
      'atomic_int', 'atomic_uint',
      'void', 'size_t',
      // Texture types
      'texture1d', 'texture2d', 'texture3d',
      'texture2d_array', 'texturecube', 'texture2d_ms',
      'depth2d', 'depth2d_array', 'depthcube',
      'sampler',
      // Metal buffer types
      'command_buffer', 'render_command_encoder', 'compute_command_encoder',
    ],

    operators: [
      '=', '>', '<', '!', '~', '?', ':',
      '==', '<=', '>=', '!=', '&&', '||', '++', '--',
      '+', '-', '*', '/', '&', '|', '^', '%', '<<', '>>',
      '+=', '-=', '*=', '/=', '&=', '|=', '^=', '%=', '<<=', '>>=',
    ],

    brackets: [
      { open: '{', close: '}', token: 'delimiter.curly' },
      { open: '[', close: ']', token: 'delimiter.square' },
      { open: '(', close: ')', token: 'delimiter.parenthesis' },
      { open: '<', close: '>', token: 'delimiter.angle' },
    ],

    tokenizer: {
      root: [
        // Comments
        [/\/\/.*$/, 'comment'],
        [/\/\*/, 'comment', '@comment'],

        // Attribute qualifiers [[ ... ]]
        [/\[\[/, 'annotation', '@attribute'],

        // Preprocessor
        [/#\s*\w+/, 'keyword.control'],

        // Numbers
        [/0[xX][0-9a-fA-F]+[hHuUlLfF]?/, 'number.hex'],
        [/\d+\.\d*([eE][\-+]?\d+)?[hHfF]?/, 'number.float'],
        [/\d+[hHuUlLfF]?/, 'number'],

        // Strings
        [/"([^"\\]|\\.)*$/, 'string.invalid'],
        [/"/, 'string', '@string'],

        // Identifiers and keywords
        [/[a-zA-Z_]\w*/, {
          cases: {
            '@keywords': 'keyword',
            '@typeKeywords': 'type',
            '@default': 'identifier',
          },
        }],

        // Operators
        [/[{}()\[\]]/, '@brackets'],
        [/[<>]/, 'operator'],
        [/[;,.]/, 'delimiter'],
        [/[+\-*/%&|^~!=<>?:]/, 'operator'],
      ],

      comment: [
        [/[^\/*]+/, 'comment'],
        [/\*\//, 'comment', '@pop'],
        [/[\/*]/, 'comment'],
      ],

      string: [
        [/[^\\"]+/, 'string'],
        [/\\./, 'string.escape'],
        [/"/, 'string', '@pop'],
      ],

      attribute: [
        [/\]\]/, 'annotation', '@pop'],
        [/[a-zA-Z_]\w*/, 'annotation'],
        [/[,():]/, 'delimiter'],
        [/\d+/, 'number'],
        [/./, 'annotation'],
      ],
    },
  });
}
