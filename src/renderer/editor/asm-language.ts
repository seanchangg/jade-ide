import * as monaco from 'monaco-editor';

export function registerAsmLanguage(): void {
  // Register language
  monaco.languages.register({ id: 'asm' });

  // Tokenizer — Intel syntax (x86-64) + ARM64
  monaco.languages.setMonarchTokensProvider('asm', {
    defaultToken: '',
    ignoreCase: true,

    // x86-64 registers
    x86regs: [
      'rax','rbx','rcx','rdx','rsi','rdi','rbp','rsp',
      'r8','r9','r10','r11','r12','r13','r14','r15',
      'eax','ebx','ecx','edx','esi','edi','ebp','esp',
      'ax','bx','cx','dx','si','di','bp','sp',
      'al','bl','cl','dl','ah','bh','ch','dh',
      'sil','dil','bpl','spl',
      'r8d','r9d','r10d','r11d','r12d','r13d','r14d','r15d',
      'r8w','r9w','r10w','r11w','r12w','r13w','r14w','r15w',
      'r8b','r9b','r10b','r11b','r12b','r13b','r14b','r15b',
      'xmm0','xmm1','xmm2','xmm3','xmm4','xmm5','xmm6','xmm7',
      'xmm8','xmm9','xmm10','xmm11','xmm12','xmm13','xmm14','xmm15',
      'ymm0','ymm1','ymm2','ymm3','ymm4','ymm5','ymm6','ymm7',
      'ymm8','ymm9','ymm10','ymm11','ymm12','ymm13','ymm14','ymm15',
      'zmm0','zmm1','zmm2','zmm3','zmm4','zmm5','zmm6','zmm7',
      'zmm8','zmm9','zmm10','zmm11','zmm12','zmm13','zmm14','zmm15',
      'rip',
    ],

    // ARM64 registers
    armregs: [
      'x0','x1','x2','x3','x4','x5','x6','x7','x8','x9',
      'x10','x11','x12','x13','x14','x15','x16','x17','x18','x19',
      'x20','x21','x22','x23','x24','x25','x26','x27','x28','x29','x30',
      'w0','w1','w2','w3','w4','w5','w6','w7','w8','w9',
      'w10','w11','w12','w13','w14','w15','w16','w17','w18','w19',
      'w20','w21','w22','w23','w24','w25','w26','w27','w28','w29','w30',
      'sp','lr','xzr','wzr','fp',
      'd0','d1','d2','d3','d4','d5','d6','d7',
      'd8','d9','d10','d11','d12','d13','d14','d15',
      's0','s1','s2','s3','s4','s5','s6','s7',
      's8','s9','s10','s11','s12','s13','s14','s15',
      'q0','q1','q2','q3','q4','q5','q6','q7',
      'v0','v1','v2','v3','v4','v5','v6','v7',
    ],

    // Memory size qualifiers
    memsize: [
      'byte','word','dword','qword','xmmword','ymmword','ptr',
    ],

    tokenizer: {
      root: [
        // Comments
        [/;.*$/, 'comment'],
        [/@.*$/, 'comment'],
        [/\/\/.*$/, 'comment'],

        // Function labels (demangled names, start with _ or letter, not . or LBB)
        [/^[a-zA-Z_][a-zA-Z0-9_<>,&*: ()\[\]~]+:/, 'type.identifier'],
        // Branch labels (.LBB0_1:, LBB0_1:, .L37:, etc.) — dimmer
        [/^[.\s]*L[A-Z]*\w*:/, 'comment'],
        // Other labels starting with .
        [/^\.[a-zA-Z_]\w*:/, 'comment'],

        // Data directives
        [/\.(long|quad|byte|short|zero|ascii|asciz|string|space|word|set)\b/, 'keyword'],

        // Hex numbers
        [/0x[0-9a-fA-F]+/, 'number.hex'],
        [/#-?0x[0-9a-fA-F]+/, 'number.hex'],

        // Decimal numbers (including negative and #-prefixed for ARM)
        [/#-?\d+/, 'number'],
        [/\b-?\d+\b/, 'number'],

        // Memory size qualifiers (QWORD PTR, etc.)
        [/\b(BYTE|WORD|DWORD|QWORD|XMMWORD|YMMWORD|OWORD)\b/, 'keyword.control'],
        [/\bPTR\b/, 'keyword.control'],

        // Registers — match word boundaries
        [/\b(rax|rbx|rcx|rdx|rsi|rdi|rbp|rsp|rip|r8|r9|r1[0-5]|r[89][dwb]|r1[0-5][dwb]|eax|ebx|ecx|edx|esi|edi|ebp|esp|[abcd][lhx]|[sd]il?|[bs]pl?|[xy]mm[0-9]{1,2}|zmm[0-9]{1,2})\b/, 'variable.name'],

        // ARM registers
        [/\b([xw](3[0-1]|[12][0-9]|[0-9])|sp|lr|xzr|wzr|fp|[dsqv]([12]?[0-9]|3[01]))\b/, 'variable.name'],

        // Branch/jump instructions
        [/\b(jmp|je|jne|jz|jnz|jg|jge|jl|jle|ja|jae|jb|jbe|jc|jnc|jo|jno|js|jns|call|ret|leave|b|bl|br|blr|b\.\w+|cb[n]?z|tb[n]?z|ret)\b/, 'keyword'],

        // Common instructions
        [/\b(mov|movsx|movzx|movsxd|movabs|movsd|movss|movaps|movups|movdqa|movdqu|lea|push|pop|xchg|cmov\w+|movsd|movss|ldr|ldp|str|stp|adr|adrp|ldur|stur)\b/, 'entity.name.function'],

        // Arithmetic
        [/\b(add|sub|mul|imul|div|idiv|neg|inc|dec|adc|sbb|and|or|xor|not|shl|shr|sar|sal|rol|ror|test|cmp|madd|msub|sdiv|udiv|fmadd|fmsub)\b/, 'entity.name.function'],

        // SIMD/vector instructions
        [/\b(v?add[ps][sd]|v?sub[ps][sd]|v?mul[ps][sd]|v?div[ps][sd]|v?fmadd[0-9]*[ps][sd]|v?fmsub[0-9]*[ps][sd]|v?fnmadd[0-9]*[ps][sd]|v?sqrt[ps][sd]|v?rsqrt[ps]s|v?max[ps][sd]|v?min[ps][sd]|v?cmp[ps][sd]|v?broadcast[sf][sd]|v?perm\w+|v?blend\w+|v?shuf\w+|v?unpck\w+|v?cvt\w+|fadd|fsub|fmul|fdiv|fmov|fcmp|fcvt\w*|scvtf|ucvtf)\b/, 'string'],

        // Nop, fence, etc.
        [/\b(nop|hlt|int|syscall|endbr64|ud2|mfence|lfence|sfence|dmb|dsb|isb)\b/, 'comment'],

        // Memory references [rbp-24] or [sp, #16]
        [/\[/, 'delimiter.bracket'],
        [/\]/, 'delimiter.bracket'],

        // Symbols/identifiers (function names after call, etc.)
        [/[a-zA-Z_][a-zA-Z0-9_@.]*/, 'identifier'],
      ],
    },
  });
}
