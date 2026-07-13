import { ipcMain, BrowserWindow } from 'electron';
import { spawn, execSync, ChildProcess } from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import { IPC, BuildResult, AllocationEvent, TrainingScalar, TrainingTimingEvent } from '../shared/types';
import { getTelemetryServer } from './telemetry-server';

let runningProcess: ChildProcess | null = null;

// CMake build directory (kept separate from CLion's cmake-build-debug)
const BUILD_DIR_NAME = 'cmake-build-forge';

// buildDir → configure-args key, so we only re-run `cmake -S -B` when flags change
// (cmake --build re-generates on CMakeLists.txt edits by itself)
const configuredBuilds = new Map<string, string>();

// Path to bundled instrumentation C files
const FORGE_TRACE_C = path.join(__dirname, '..', '..', '..', 'include', 'forge_trace.cpp');
const FORGE_INTERPOSE_C = path.join(__dirname, '..', '..', '..', 'include', 'forge_interpose.c');
const FORGE_INTERPOSE_DYLIB = '/tmp/forge_interpose.dylib';

// Metal telemetry probe: interposes MTLCreateSystemDefaultDevice and swizzles
// the device to auto-discover GPU buffers, capture per-command-buffer GPU
// timings, and stream (checked) VRAM buffers over the telemetry socket — no
// __FORGE_* instrumentation required. See probe/README.md.
const FORGE_PROBE_MM = path.join(__dirname, '..', '..', '..', 'probe', 'forge_probe.mm');
export const FORGE_PROBE_DYLIB = '/tmp/forge_probe.dylib';

export function ensureProbeDylib(): boolean {
  try {
    if (!fs.existsSync(FORGE_PROBE_MM)) return false;
    let needsCompile = !fs.existsSync(FORGE_PROBE_DYLIB);
    if (!needsCompile) {
      needsCompile =
        fs.statSync(FORGE_PROBE_MM).mtimeMs > fs.statSync(FORGE_PROBE_DYLIB).mtimeMs;
    }
    if (needsCompile) {
      execSync(
        `clang++ -dynamiclib -fobjc-arc -O2 -framework Metal -framework Foundation ` +
          `-o ${FORGE_PROBE_DYLIB} ${FORGE_PROBE_MM}`,
        { timeout: 30000 }
      );
    }
    return true;
  } catch {
    return false;
  }
}

function parseCompilerErrors(output: string, cwd: string): BuildResult['errors'] {
  const errors: BuildResult['errors'] = [];
  // Match: file:line:col: severity: message
  const pattern = /^(.+?):(\d+):(\d+):\s+(error|warning|note):\s+(.+)$/gm;
  let match;

  while ((match = pattern.exec(output)) !== null) {
    errors.push({
      file: path.isAbsolute(match[1]) ? match[1] : path.join(cwd, match[1]),
      line: parseInt(match[2], 10),
      column: parseInt(match[3], 10),
      message: match[5],
      severity: match[4] as 'error' | 'warning' | 'note',
    });
  }

  // Match CMake configure errors: "CMake Error at CMakeLists.txt:12 (add_executable):"
  const cmakePattern = /^CMake (Error|Warning)(?: \(dev\))? at (.+?):(\d+)(?:\s+\([^)]*\))?:\s*\n((?:.+\n?)*?)(?=\n\n|\n?$)/gm;
  while ((match = cmakePattern.exec(output)) !== null) {
    errors.push({
      file: path.isAbsolute(match[2]) ? match[2] : path.join(cwd, match[2]),
      line: parseInt(match[3], 10),
      column: 1,
      message: match[4].trim().replace(/\s+/g, ' '),
      severity: match[1] === 'Error' ? 'error' : 'warning',
    });
  }

  return errors;
}

// Parse idetools.h output from program stdout/stderr
// Protocol: lines starting with __FORGE_ are instrumentation data
function parseForgeOutput(
  line: string,
  win: BrowserWindow
): boolean {
  if (!line.startsWith('__FORGE_')) return false;

  if (line.startsWith('__FORGE_ALLOC|') || line.startsWith('__FORGE_FREE|')) {
    // __FORGE_ALLOC|pointer|size|file|line|timestamp
    // __FORGE_FREE|pointer|size|file|line|timestamp
    const parts = line.split('|');
    if (parts.length >= 6) {
      const event: AllocationEvent = {
        type: parts[0] === '__FORGE_ALLOC' ? 'alloc' : 'free',
        pointer: parts[1],
        size: parseInt(parts[2], 10),
        file: parts[3],
        line: parseInt(parts[4], 10),
        timestamp: parseFloat(parts[5]),
      };
      win.webContents.send(IPC.BUILD_MEMORY_EVENT, event);
    }
    return true;
  }

  if (line.startsWith('__FORGE_SCALAR|')) {
    // __FORGE_SCALAR|name|step|value|timestamp
    const parts = line.split('|');
    if (parts.length >= 5) {
      const scalar: TrainingScalar = {
        name: parts[1],
        step: parseInt(parts[2], 10),
        value: parseFloat(parts[3]),
        timestamp: parseFloat(parts[4]),
      };
      // Route through the telemetry registry so a scalar first seen via legacy
      // stdout gets auto-registered exactly like a socket `decl`.
      const ts = getTelemetryServer();
      if (ts) ts.ingestScalar(scalar);
      else win.webContents.send(IPC.BUILD_TRAINING_SCALAR, scalar);
    }
    return true;
  }

  if (line.startsWith('__FORGE_TIMING|')) {
    // __FORGE_TIMING|name|duration_ms|step
    const parts = line.split('|');
    if (parts.length >= 4) {
      const timing: TrainingTimingEvent = {
        name: parts[1],
        durationMs: parseFloat(parts[2]),
        step: parseInt(parts[3], 10),
      };
      const ts = getTelemetryServer();
      if (ts) ts.ingestTiming(timing);
      else win.webContents.send(IPC.BUILD_TRAINING_TIMING, timing);
    }
    return true;
  }

  return false;
}

// Parse AddressSanitizer output for leak/error information
function parseAsanOutput(output: string, win: BrowserWindow): void {
  // Parse leak summary: "SUMMARY: AddressSanitizer: N byte(s) leaked in M allocation(s)"
  const summaryMatch = output.match(
    /SUMMARY:\s*AddressSanitizer:\s*(\d+)\s*byte\(s\)\s*leaked\s*in\s*(\d+)\s*allocation\(s\)/
  );
  if (summaryMatch) {
    const leakedBytes = parseInt(summaryMatch[1], 10);
    const leakedAllocations = parseInt(summaryMatch[2], 10);
    win.webContents.send(IPC.BUILD_MEMORY_EVENT, {
      type: 'asan-leak-summary',
      leakedBytes,
      leakedAllocations,
    });
  }

  // Parse individual leak entries with file:line info
  // Pattern: #N 0xADDR in func file.cpp:line
  const leakEntryPattern = /#\d+\s+0x[0-9a-f]+\s+in\s+(\S+)\s+(\S+?):(\d+)/gi;
  let leakMatch;
  while ((leakMatch = leakEntryPattern.exec(output)) !== null) {
    win.webContents.send(IPC.BUILD_MEMORY_EVENT, {
      type: 'asan-leak-location',
      functionName: leakMatch[1],
      file: leakMatch[2],
      line: parseInt(leakMatch[3], 10),
    });
  }

  // Parse ASan error types: heap-use-after-free, stack-buffer-overflow, etc.
  const errorTypePattern = /ERROR:\s*AddressSanitizer:\s*([\w-]+)/g;
  let errorMatch;
  while ((errorMatch = errorTypePattern.exec(output)) !== null) {
    const errorType = errorMatch[1];
    win.webContents.send(IPC.BUILD_OUTPUT,
      `\x1b[31m[ASan] ${errorType}\x1b[0m\n`
    );
  }
}

// Parse ASan allocation stats from stderr after process exit
function parseAsanStats(output: string, win: BrowserWindow): void {
  // ASan stats format varies, but commonly:
  // "Stats: NM mallocs, NF frees, TB total bytes"
  const statsPattern = /Stats:\s*(\d+)M?\s*mallocs?,\s*(\d+)F?\s*frees?,\s*(\d+)\s*total/i;
  const statsMatch = output.match(statsPattern);
  if (statsMatch) {
    win.webContents.send(IPC.BUILD_MEMORY_EVENT, {
      type: 'asan-stats',
      totalAllocations: parseInt(statsMatch[1], 10),
      totalFrees: parseInt(statsMatch[2], 10),
      totalBytes: parseInt(statsMatch[3], 10),
    });
  }

  // Also try the print_stats=1 format:
  // "number of allocations  : N"
  // "number of deallocations: N"
  // "bytes allocated        : N"
  const allocCountMatch = output.match(/number of allocations\s*:\s*(\d+)/);
  const freeCountMatch = output.match(/number of deallocations\s*:\s*(\d+)/);
  const bytesAllocMatch = output.match(/bytes allocated\s*:\s*(\d+)/);
  const bytesFreedMatch = output.match(/bytes freed\s*:\s*(\d+)/);

  if (allocCountMatch || freeCountMatch || bytesAllocMatch) {
    win.webContents.send(IPC.BUILD_MEMORY_EVENT, {
      type: 'asan-stats',
      totalAllocations: allocCountMatch ? parseInt(allocCountMatch[1], 10) : 0,
      totalFrees: freeCountMatch ? parseInt(freeCountMatch[1], 10) : 0,
      totalBytes: bytesAllocMatch ? parseInt(bytesAllocMatch[1], 10) : 0,
      totalFreedBytes: bytesFreedMatch ? parseInt(bytesFreedMatch[1], 10) : 0,
    });
  }
}

// Parse __FORGE_HEAP_SUMMARY from malloc interposer
function parseHeapSummary(line: string, win: BrowserWindow): boolean {
  if (!line.startsWith('__FORGE_HEAP_SUMMARY|')) return false;

  // __FORGE_HEAP_SUMMARY|total_alloc|total_freed|current_heap|peak_heap|alloc_count|free_count
  const parts = line.split('|');
  console.log('[forge-main] heap summary parts:', parts);
  if (parts.length >= 7) {
    const payload = {
      type: 'heap-summary',
      totalAlloc: parseInt(parts[1], 10),
      totalFreed: parseInt(parts[2], 10),
      currentHeap: parseInt(parts[3], 10),
      peakHeap: parseInt(parts[4], 10),
      allocCount: parseInt(parts[5], 10),
      freeCount: parseInt(parts[6], 10),
    };
    console.log('[forge-main] sending heap-summary:', JSON.stringify(payload));
    win.webContents.send(IPC.BUILD_MEMORY_EVENT, payload);
  }
  return true;
}

// Walk up from the file's directory looking for a CMakeLists.txt project root.
// Stops at $HOME (or after 8 levels) so a stray ancestor project isn't picked up.
function findCMakeRoot(startDir: string): string | null {
  const home = process.env.HOME || '/';
  let dir = startDir;
  for (let depth = 0; depth < 8; depth++) {
    if (fs.existsSync(path.join(dir, 'CMakeLists.txt'))) return dir;
    if (dir === home || dir === '/') break;
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

// Generate a CMakeLists.txt for a directory that has none, modeled on
// ClionProjects/metalllm: one executable target plus a custom-command chain
// that compiles sibling .metal shaders into default.metallib in the build dir.
function generateCMakeLists(cwd: string, activeFile: string, win: BrowserWindow): boolean {
  const ext = path.extname(activeFile).toLowerCase();

  // Pick the executable's source: the active file itself, or for a .metal
  // file the host program next to it (main.cpp / main.mm / a lone .cpp/.mm).
  let sourceFile: string | null = null;
  if (ext === '.metal') {
    const siblings = fs.readdirSync(cwd);
    sourceFile =
      ['main.cpp', 'main.mm', 'main.cc', 'main.m'].find(f => siblings.includes(f)) ||
      siblings.filter(f => /\.(cpp|cc|mm|m)$/.test(f)).sort()[0] || null;
  } else {
    sourceFile = path.basename(activeFile);
  }

  const isObjC = sourceFile ? /\.(mm|m)$/.test(sourceFile) : false;
  const hasMetal = fs.readdirSync(cwd).some(f => f.endsWith('.metal'));
  const rawName = sourceFile
    ? path.basename(sourceFile, path.extname(sourceFile))
    : path.basename(cwd);
  const target = rawName.replace(/[^A-Za-z0-9_]/g, '_') || 'app';

  const lines: string[] = [
    'cmake_minimum_required(VERSION 3.20)',
    `project(${target} ${isObjC ? 'OBJCXX CXX' : 'CXX'})`,
    '',
    'set(CMAKE_CXX_STANDARD 17)',
    'set(CMAKE_CXX_STANDARD_REQUIRED ON)',
    '',
  ];

  if (sourceFile) {
    lines.push(
      `add_executable(${target} ${sourceFile})`,
      '',
      '# Forge IDE instrumentation headers (idetools.h), passed at configure time',
      'if(DEFINED FORGE_INCLUDE_DIR)',
      `    target_include_directories(${target} PRIVATE \${FORGE_INCLUDE_DIR})`,
      'endif()',
      ''
    );
    if (isObjC || hasMetal) {
      lines.push(
        `target_link_libraries(${target} PRIVATE`,
        '        "-framework Metal"',
        '        "-framework MetalPerformanceShaders"',
        '        "-framework Foundation"',
        '        "-framework QuartzCore"',
        ')',
        ''
      );
    }
  }

  if (hasMetal) {
    lines.push(
      '# ── Compile any sibling .metal shaders into default.metallib at build time ──',
      'file(GLOB METAL_SHADERS "${CMAKE_SOURCE_DIR}/*.metal")',
      'if(METAL_SHADERS)',
      '    set(AIR_FILES "")',
      '    foreach(SHADER ${METAL_SHADERS})',
      '        get_filename_component(SHADER_NAME ${SHADER} NAME_WE)',
      '        set(AIR_FILE "${CMAKE_BINARY_DIR}/${SHADER_NAME}.air")',
      '        add_custom_command(',
      '                OUTPUT ${AIR_FILE}',
      '                COMMAND xcrun -sdk macosx metal -c ${SHADER} -o ${AIR_FILE}',
      '                DEPENDS ${SHADER}',
      '                COMMENT "Compiling Metal shader ${SHADER_NAME}.metal"',
      '        )',
      '        list(APPEND AIR_FILES ${AIR_FILE})',
      '    endforeach()',
      '',
      '    set(METALLIB "${CMAKE_BINARY_DIR}/default.metallib")',
      '    add_custom_command(',
      '            OUTPUT ${METALLIB}',
      '            COMMAND xcrun -sdk macosx metallib ${AIR_FILES} -o ${METALLIB}',
      '            DEPENDS ${AIR_FILES}',
      '            COMMENT "Linking default.metallib"',
      '    )',
      `    add_custom_target(metal_shaders ${sourceFile ? '' : 'ALL '}DEPENDS \${METALLIB})`,
      ...(sourceFile ? [`    add_dependencies(${target} metal_shaders)`] : []),
      'endif()',
      ''
    );
  }

  if (!sourceFile && !hasMetal) return false;

  try {
    fs.writeFileSync(path.join(cwd, 'CMakeLists.txt'), lines.join('\n'));
    win.webContents.send(IPC.BUILD_OUTPUT,
      `\x1b[36m[forge]\x1b[0m No CMakeLists.txt found — generated one for target '${target}'\n`
    );
    return true;
  } catch (err: any) {
    win.webContents.send(IPC.BUILD_OUTPUT,
      `\x1b[31m[cmake]\x1b[0m Failed to write CMakeLists.txt: ${err.message}\n`
    );
    return false;
  }
}

// Run a cmake command, streaming output to the terminal and collecting it
function runCMake(args: string[], cwd: string, win: BrowserWindow): Promise<{ code: number; output: string }> {
  return new Promise((resolve) => {
    const proc = spawn('cmake', args, { cwd, env: { ...process.env } });
    let output = '';
    const onData = (data: Buffer) => {
      const text = data.toString();
      output += text;
      win.webContents.send(IPC.BUILD_OUTPUT, text.replace(/\n/g, '\r\n'));
    };
    proc.stdout.on('data', onData);
    proc.stderr.on('data', onData);
    proc.on('close', (code) => resolve({ code: code ?? -1, output }));
    proc.on('error', (err) => resolve({ code: -1, output: `Failed to start cmake: ${err.message}` }));
  });
}

// Locate the built executable via CMake's file API (codemodel-v2 reply).
// Prefers the executable target whose sources include the active file.
function findBuiltExecutable(buildDir: string, sourceRoot: string, activeFile: string): string | undefined {
  try {
    const replyDir = path.join(buildDir, '.cmake', 'api', 'v1', 'reply');
    const indexFile = fs.readdirSync(replyDir)
      .filter(f => f.startsWith('index-') && f.endsWith('.json'))
      .sort()
      .pop();
    if (!indexFile) return undefined;

    const index = JSON.parse(fs.readFileSync(path.join(replyDir, indexFile), 'utf-8'));
    const cmRef = (index.reply?.['codemodel-v2'])
      || Object.values(index.reply || {}).find((r: any) => r?.kind === 'codemodel');
    if (!cmRef?.jsonFile) return undefined;

    const codemodel = JSON.parse(fs.readFileSync(path.join(replyDir, cmRef.jsonFile), 'utf-8'));
    const config = codemodel.configurations?.[0];
    if (!config) return undefined;

    const executables: Array<{ artifact: string; sources: string[] }> = [];
    for (const t of config.targets || []) {
      const tj = JSON.parse(fs.readFileSync(path.join(replyDir, t.jsonFile), 'utf-8'));
      if (tj.type !== 'EXECUTABLE' || !tj.artifacts?.[0]?.path) continue;
      executables.push({
        artifact: path.resolve(buildDir, tj.artifacts[0].path),
        sources: (tj.sources || []).map((s: any) => path.resolve(sourceRoot, s.path)),
      });
    }
    if (executables.length === 0) return undefined;

    const owning = executables.find(e => e.sources.includes(activeFile));
    return (owning || executables[0]).artifact;
  } catch {
    return undefined;
  }
}

async function compile(
  filePath: string,
  flags: string[],
  win: BrowserWindow,
  sanitize?: boolean,
  instrument?: boolean
): Promise<BuildResult> {
  const cwd = path.dirname(filePath);
  const startTime = Date.now();
  const forgeIncludeDir = path.join(__dirname, '..', '..', '..', 'include');

  // ── Resolve (or bootstrap) the CMake project ──
  let root = findCMakeRoot(cwd);
  if (!root) {
    if (!generateCMakeLists(cwd, filePath, win)) {
      return {
        success: false,
        errors: [{
          file: filePath, line: 0, column: 0,
          message: 'No CMakeLists.txt found and no buildable source to generate one from',
          severity: 'error',
        }],
        duration: Date.now() - startTime,
      };
    }
    root = cwd;
  }
  const buildDir = path.join(root, BUILD_DIR_NAME);

  // ── Flags → CMake cache variables ──
  const cxxFlags = ['-g', ...flags];
  const ldFlags: string[] = [];
  if (sanitize) {
    cxxFlags.push('-fsanitize=address', '-fno-omit-frame-pointer');
    ldFlags.push('-fsanitize=address');
  }
  if (instrument) {
    cxxFlags.push('-fprofile-arcs', '-ftest-coverage');
    ldFlags.push('--coverage');
  }

  const configureArgs = [
    '-S', root,
    '-B', buildDir,
    '-DCMAKE_BUILD_TYPE=Debug',
    '-DCMAKE_EXPORT_COMPILE_COMMANDS=ON',
    `-DFORGE_INCLUDE_DIR=${forgeIncludeDir}`,
    `-DCMAKE_CXX_FLAGS=${cxxFlags.join(' ')}`,
    `-DCMAKE_OBJCXX_FLAGS=${cxxFlags.join(' ')}`,
    `-DCMAKE_EXE_LINKER_FLAGS=${ldFlags.join(' ')}`,
  ];

  // ── Configure (skipped when args unchanged and cache exists) ──
  const configureKey = configureArgs.join('\x1f');
  const needsConfigure =
    configuredBuilds.get(buildDir) !== configureKey ||
    !fs.existsSync(path.join(buildDir, 'CMakeCache.txt'));

  if (needsConfigure) {
    // File API query so the reply tells us where executable artifacts land
    try {
      const queryDir = path.join(buildDir, '.cmake', 'api', 'v1', 'query');
      fs.mkdirSync(queryDir, { recursive: true });
      fs.writeFileSync(path.join(queryDir, 'codemodel-v2'), '');
    } catch {}

    win.webContents.send(IPC.BUILD_OUTPUT, `\x1b[36m[cmake]\x1b[0m Configuring ${root}\n`);
    const cfg = await runCMake(configureArgs, root, win);
    if (cfg.code !== 0) {
      configuredBuilds.delete(buildDir);
      return {
        success: false,
        errors: parseCompilerErrors(cfg.output, root),
        duration: Date.now() - startTime,
      };
    }
    configuredBuilds.set(buildDir, configureKey);
  }

  // ── Build ──
  const build = await runCMake(['--build', buildDir, '--parallel'], root, win);
  const errors = parseCompilerErrors(build.output, root);
  if (build.code !== 0) {
    return { success: false, errors, duration: Date.now() - startTime };
  }

  const executable = findBuiltExecutable(buildDir, root, filePath);
  return {
    success: true,
    executable,
    errors,
    duration: Date.now() - startTime,
  };
}

// Recursively collect files with a given extension (CMake nests .gcda/.gcno
// under CMakeFiles/<target>.dir/)
function findFilesRecursive(dir: string, ext: string, maxDepth = 6): string[] {
  const results: string[] = [];
  if (maxDepth < 0) return results;
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return results;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      results.push(...findFilesRecursive(full, ext, maxDepth - 1));
    } else if (entry.name.endsWith(ext)) {
      results.push(full);
    }
  }
  return results;
}

// Ensure the malloc interposer dylib is compiled and up-to-date
function ensureInterposeDylib(): boolean {
  try {
    // Check if source exists
    if (!fs.existsSync(FORGE_INTERPOSE_C)) return false;

    // Recompile if dylib is missing or older than source
    let needsCompile = !fs.existsSync(FORGE_INTERPOSE_DYLIB);
    if (!needsCompile) {
      const srcStat = fs.statSync(FORGE_INTERPOSE_C);
      const dylibStat = fs.statSync(FORGE_INTERPOSE_DYLIB);
      needsCompile = srcStat.mtimeMs > dylibStat.mtimeMs;
    }

    if (needsCompile) {
      execSync(
        `clang -shared -o ${FORGE_INTERPOSE_DYLIB} ${FORGE_INTERPOSE_C} -ldl`,
        { timeout: 15000 }
      );
    }
    return true;
  } catch {
    return false;
  }
}

async function run(
  config: { executable: string; args?: string[]; enableSanitizers?: boolean; enableInstrumentation?: boolean },
  win: BrowserWindow
): Promise<any> {
  if (runningProcess) {
    runningProcess.kill();
    runningProcess = null;
  }

  const cwd = path.dirname(config.executable);
  const startTime = Date.now();
  const executedLines = new Map<number, number>();

  // Instrumentation tracking for -finstrument-functions
  const funcAddresses = new Set<string>();
  let maxCallDepth = 0;
  let currentCallDepth = 0;

  return new Promise((resolve) => {
    const env: Record<string, string> = { ...process.env as Record<string, string> };

    // Advertise the telemetry socket so the program (and any injected probe
    // dylib) can auto-report scalars/timers/GPU buffers over NDJSON.
    const sockPath = getTelemetryServer()?.socketPath;
    if (sockPath) env.FORGE_TELEMETRY_SOCK = sockPath;

    // Enable address sanitizer runtime options
    if (config.enableSanitizers) {
      env.ASAN_OPTIONS = 'detect_leaks=0:print_stats=1';
    }

    // Injected dylibs (colon-separated for DYLD_INSERT_LIBRARIES):
    //  • malloc interposer — memory tracking; conflicts with ASan, so skipped
    //    when sanitizers are on.
    //  • Metal telemetry probe — GPU buffer/timing auto-discovery; safe with
    //    ASan, so always attempted.
    const injectedDylibs: string[] = [];
    let interposeActive = false;
    if (!config.enableSanitizers && ensureInterposeDylib()) {
      injectedDylibs.push(FORGE_INTERPOSE_DYLIB);
      interposeActive = true;
    }
    if (ensureProbeDylib()) {
      injectedDylibs.push(FORGE_PROBE_DYLIB);
    }
    if (injectedDylibs.length > 0) {
      env.DYLD_INSERT_LIBRARIES = injectedDylibs.join(':');
    }

    runningProcess = spawn(config.executable, config.args || [], { cwd, env });

    let stdoutBuffer = '';
    let sanitizerOutput = '';
    let stderrBuffer = '';

    runningProcess.stdout?.on('data', (data: Buffer) => {
      const text = data.toString();
      stdoutBuffer += text;

      // Process line by line for forge protocol
      const lines = stdoutBuffer.split('\n');
      stdoutBuffer = lines.pop() || ''; // keep incomplete line

      for (const line of lines) {
        if (!parseForgeOutput(line.trim(), win)) {
          // Regular output — send to terminal/output
          win.webContents.send(IPC.BUILD_OUTPUT, line + '\n');
        }
      }
    });

    runningProcess.stderr?.on('data', (data: Buffer) => {
      const text = data.toString();
      sanitizerOutput += text;
      stderrBuffer += text;

      // Process line by line
      const lines = stderrBuffer.split('\n');
      stderrBuffer = lines.pop() || ''; // keep incomplete line

      for (const line of lines) {
        // Check for -finstrument-functions trace output (old format)
        if (line.startsWith('__FORGE_TRACE|')) {
          const parts = line.split('|');
          if (parts.length >= 3) {
            const traceLineNum = parseInt(parts[2], 10);
          executedLines.set(traceLineNum, (executedLines.get(traceLineNum) || 0) + 1);
          }
        }
        // Parse __FORGE_FUNC_ENTER from forge_trace.c
        else if (line.startsWith('__FORGE_FUNC_ENTER|')) {
          const parts = line.split('|');
          if (parts.length >= 3) {
            funcAddresses.add(parts[1]); // track unique function address
            currentCallDepth++;
            if (currentCallDepth > maxCallDepth) {
              maxCallDepth = currentCallDepth;
            }
          }
        }
        // Parse __FORGE_FUNC_EXIT from forge_trace.c
        else if (line.startsWith('__FORGE_FUNC_EXIT|')) {
          currentCallDepth = Math.max(0, currentCallDepth - 1);
        }
        // Parse heap summary from malloc interposer
        else if (line.startsWith('__FORGE_HEAP_SUMMARY|')) {
          parseHeapSummary(line, win);
        }
        // Skip interposer active marker
        else if (line.startsWith('__FORGE_INTERPOSE_ACTIVE')) {
          // Interposer is active — nothing to forward
        }
        // Regular stderr — forward to output
        else if (line.length > 0) {
          win.webContents.send(IPC.BUILD_OUTPUT, line + '\n');
        }
      }
    });

    runningProcess.on('close', (code) => {
      runningProcess = null;

      // Flush remaining stdout buffer
      if (stdoutBuffer.trim()) {
        if (!parseForgeOutput(stdoutBuffer.trim(), win)) {
          win.webContents.send(IPC.BUILD_OUTPUT, stdoutBuffer + '\n');
        }
      }

      // Flush remaining stderr buffer
      if (stderrBuffer.trim()) {
        const line = stderrBuffer.trim();
        if (line.startsWith('__FORGE_HEAP_SUMMARY|')) {
          parseHeapSummary(line, win);
        } else if (!line.startsWith('__FORGE_FUNC_ENTER|') &&
                   !line.startsWith('__FORGE_FUNC_EXIT|') &&
                   !line.startsWith('__FORGE_INTERPOSE_ACTIVE')) {
          win.webContents.send(IPC.BUILD_OUTPUT, line + '\n');
        }
      }

      // Parse sanitizer output after process exits
      if (config.enableSanitizers && sanitizerOutput) {
        parseAsanOutput(sanitizerOutput, win);
        parseAsanStats(sanitizerOutput, win);
      }

      // Parse coverage data using gcov
      if (config.enableInstrumentation) {
        try {
          // Find .gcda files under the build dir (CMake nests them in CMakeFiles/)
          const gcdaFiles = findFilesRecursive(cwd, '.gcda');
          if (gcdaFiles.length > 0) {
            // Run gcov on the first .gcda file
            const gcovResult = execSync(
              `gcov -o "${path.dirname(gcdaFiles[0])}" "${gcdaFiles[0]}"`,
              { cwd, timeout: 10000 }
            ).toString();

            // Parse the .gcov output files
            const gcovFiles = fs.readdirSync(cwd).filter(f => f.endsWith('.cpp.gcov'));
            for (const gf of gcovFiles) {
              const gcovContent = fs.readFileSync(path.join(cwd, gf), 'utf-8');
              for (const gcovLine of gcovContent.split('\n')) {
                // Format: "    count:  lineNum:source" or "#####:  lineNum:source"
                const m = gcovLine.match(/^\s*(\d+):\s*(\d+):/);
                if (m) {
                  const count = parseInt(m[1], 10);
                  const lineNum = parseInt(m[2], 10);
                  if (count > 0 && lineNum > 0) {
                    executedLines.set(lineNum, count);
                  }
                }
              }
              // Clean up .gcov files
              try { fs.unlinkSync(path.join(cwd, gf)); } catch {}
            }
            // Clean up other generated .gcov files
            const allGcov = fs.readdirSync(cwd).filter(f => f.endsWith('.gcov'));
            for (const gf of allGcov) {
              try { fs.unlinkSync(path.join(cwd, gf)); } catch {}
            }
          }
          // Clean up .gcda files (keep .gcno — the build owns those)
          for (const gf of gcdaFiles) {
            try { fs.unlinkSync(gf); } catch {}
          }

          if (executedLines.size > 0) {
            const totalExecCount = Array.from(executedLines.values()).reduce((a, b) => a + b, 0);
            win.webContents.send(IPC.BUILD_OUTPUT,
              `\x1b[36m[forge]\x1b[0m Coverage: ${executedLines.size} lines executed (${totalExecCount} total hits)\n`
            );
          }
        } catch (err: any) {
          // gcov parsing failed — non-fatal
          console.error('[forge] gcov parse error:', err.message);
        }
      }

      // Always clean up stray .gcov report files (gcov writes them into cwd);
      // .gcno/.gcda live in the CMake build tree and belong to the build
      try {
        const strayGcov = fs.readdirSync(cwd).filter(f => f.endsWith('.gcov'));
        for (const f of strayGcov) {
          try { fs.unlinkSync(path.join(cwd, f)); } catch {}
        }
      } catch {}

      resolve({
        exitCode: code ?? -1,
        duration: Date.now() - startTime,
        executedLines: Object.fromEntries(executedLines),
        sanitizerOutput: sanitizerOutput || undefined,
        interposeActive,
        instrumentationSummary: funcAddresses.size > 0 ? {
          uniqueFunctions: funcAddresses.size,
          maxCallDepth,
        } : undefined,
      });
    });

    runningProcess.on('error', (err) => {
      runningProcess = null;
      resolve({
        exitCode: -1,
        duration: Date.now() - startTime,
        executedLines: {},
        sanitizerOutput: `Failed to run: ${err.message}`,
      });
    });
  });
}

function demangleCpp(asm: string): string {
  try {
    // Use c++filt to demangle all mangled symbols in one pass
    const result = execSync('c++filt', {
      input: asm,
      timeout: 5000,
      encoding: 'utf-8',
    });
    return result;
  } catch {
    return asm; // fallback to mangled if c++filt not available
  }
}

async function generateAssembly(filePath: string, extraFlags: string[]): Promise<{ success: boolean; asm: string; error?: string; asmToSource?: Record<number, number> }> {
  const cwd = path.dirname(filePath);
  const forgeIncludeDir = path.join(__dirname, '..', '..', '..', 'include');
  const flags = [
    '-std=c++17', '-O3', '-march=native', '-g',
    `-I${forgeIncludeDir}`,
    ...extraFlags,
    '-S',                    // emit assembly
    '-masm=intel',           // intel syntax (readable)
    '-fno-asynchronous-unwind-tables', // less noise
    '-fno-unwind-tables',    // less noise
    '-o', '-',               // output to stdout
    filePath,
  ];

  return new Promise((resolve) => {
    const proc = spawn('clang++', flags, { cwd });
    let stdout = '';
    let stderr = '';

    proc.stdout.on('data', (d: Buffer) => { stdout += d.toString(); });
    proc.stderr.on('data', (d: Buffer) => { stderr += d.toString(); });

    proc.on('close', (code) => {
      if (code !== 0) {
        resolve({ success: false, asm: '', error: stderr });
      } else {
        // Build source line → asm line mapping from .loc directives,
        // then filter to Godbolt-style clean output
        const rawLines = stdout.split('\n');
        const filteredLines: string[] = [];
        // Map: asmLineNumber (1-based in output) → sourceLine
        const asmToSource: Record<number, number> = {};
        let currentSourceLine = 0;

        for (const line of rawLines) {
          const trimmed = line.trim();
          if (!trimmed) continue;

          // Parse .loc directives: .loc filenum lineno [column]
          const locMatch = trimmed.match(/^\.loc\s+\d+\s+(\d+)/);
          if (locMatch) {
            currentSourceLine = parseInt(locMatch[1], 10);
            continue; // don't include .loc in output
          }

          // Keep labels (not debug/internal ones)
          if (trimmed.endsWith(':') && !trimmed.startsWith('Lfunc_') &&
              !trimmed.startsWith('Ltmp') && !trimmed.startsWith('Lcfi') &&
              !trimmed.startsWith('Lloh') && !trimmed.startsWith('LCPI') &&
              !trimmed.startsWith('.Lfunc_') && !trimmed.startsWith('.Ltmp')) {
            // Function labels: start with _ or a letter (not . or L)
            // Add blank line before them for visual separation
            const isBranchLabel = trimmed.startsWith('.') || /^L[A-Za-z]+\d/.test(trimmed);
            if (!isBranchLabel && filteredLines.length > 0) {
              filteredLines.push('');
            }
            filteredLines.push(line);
            continue;
          }
          // Keep instructions (indented, not directives or debug noise)
          if (line.startsWith('\t') && !trimmed.startsWith('.') &&
              !trimmed.startsWith(';DEBUG_VALUE') && !trimmed.startsWith('; %') &&
              !trimmed.startsWith(';;') && !trimmed.startsWith('.loh')) {
            filteredLines.push(line);
            if (currentSourceLine > 0) {
              asmToSource[filteredLines.length] = currentSourceLine;
            }
            continue;
          }
          // Keep data definitions and string literals
          if (trimmed.startsWith('.long') || trimmed.startsWith('.quad') ||
              trimmed.startsWith('.byte') || trimmed.startsWith('.short') ||
              trimmed.startsWith('.zero') || trimmed.startsWith('.ascii') ||
              trimmed.startsWith('.asciz') || trimmed.startsWith('.string') ||
              trimmed.startsWith('.space') || trimmed.startsWith('.set')) {
            filteredLines.push(line);
            if (currentSourceLine > 0) {
              asmToSource[filteredLines.length] = currentSourceLine;
            }
            continue;
          }
        }

        const demangled = demangleCpp(filteredLines.join('\n'));

        resolve({ success: true, asm: demangled, asmToSource });
      }
    });

    proc.on('error', (err) => {
      resolve({ success: false, asm: '', error: err.message });
    });
  });
}

export function registerBuildHandlers(win: BrowserWindow): void {
  ipcMain.handle(IPC.BUILD_COMPILE, async (_e, filePath: string, flags?: string[], sanitize?: boolean, instrument?: boolean) => {
    return compile(filePath, flags || [], win, sanitize, instrument);
  });

  ipcMain.handle(IPC.BUILD_RUN, async (_e, config: any) => {
    return run(config, win);
  });

  ipcMain.handle(IPC.BUILD_ASM, async (_e, filePath: string, extraFlags?: string[]) => {
    return generateAssembly(filePath, extraFlags || []);
  });

  ipcMain.on(IPC.BUILD_STOP, () => {
    if (runningProcess) {
      runningProcess.kill('SIGKILL');
      runningProcess = null;
    }
  });
}
