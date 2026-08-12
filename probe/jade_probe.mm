// jade_probe.mm — Metal telemetry probe, injected via DYLD_INSERT_LIBRARIES.
//
// Proof of concept for zero-instrumentation GPU telemetry:
//   1. Interposes MTLCreateSystemDefaultDevice / MTLCopyAllDevices to catch the
//      device, then swizzles the concrete (private) device class to observe
//      every MTLBuffer allocation — name, byte size, storage mode.
//   2. Swizzles command-queue/command-buffer creation and -commit to attach
//      completion handlers, giving per-command-buffer GPU timings for free
//      (GPUStartTime/GPUEndTime) — no __JADE_TIMING macros needed.
//   3. Reads buffer contents back: shared/managed buffers directly via
//      -contents; private (VRAM-only) buffers via a blit copy to a shared
//      staging buffer on the probe's own command queue, scheduled from the
//      app's command-buffer completion handler so we never race the GPU.
//
// Output: NDJSON telemetry lines (see docs/telemetry-protocol.md) to the unix
// socket in $JADE_TELEMETRY_SOCK, falling back to stderr with a JADEJSON|
// prefix. Set JADE_TRACK_ALL=1 to stream every buffer without IDE control
// messages (used by the standalone test).

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <objc/runtime.h>

#include <cxxabi.h>
#include <dlfcn.h>
#include <execinfo.h>
#include <mach-o/dyld.h>
#include <os/lock.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

// ─── emit ────────────────────────────────────────────────────────────────────

static int g_sock = -1;
static os_unfair_lock g_emitLock = OS_UNFAIR_LOCK_INIT;

static void emit_line(NSString *line) {
  os_unfair_lock_lock(&g_emitLock);
  if (g_sock >= 0) {
    NSString *withNl = [line stringByAppendingString:@"\n"];
    const char *bytes = withNl.UTF8String;
    write(g_sock, bytes, strlen(bytes));
  } else {
    fprintf(stderr, "JADEJSON|%s\n", line.UTF8String);
  }
  os_unfair_lock_unlock(&g_emitLock);
}

static void emit_json(NSDictionary *obj) {
  NSData *d = [NSJSONSerialization dataWithJSONObject:obj options:0 error:nil];
  if (!d) return;
  emit_line([[NSString alloc] initWithData:d encoding:NSUTF8StringEncoding]);
}

// ─── buffer registry ─────────────────────────────────────────────────────────

@interface JadeBufInfo : NSObject
@property(weak) id<MTLBuffer> buffer;
// Strong ref held while tracked, so snapshots (incl. the final at-exit flush)
// still work after the app releases the buffer.
@property(strong) id<MTLBuffer> pinned;
@property(copy) NSString *name;
@property NSUInteger length;
@property MTLStorageMode storage;
@property BOOL tracked;
// User-provided shape hint from the IDE (0 = unknown, guess a square).
@property NSUInteger shapeRows;
@property NSUInteger shapeCols;
@end
@implementation JadeBufInfo
@end

static NSMutableArray<JadeBufInfo *> *g_buffers;
static NSMutableSet<NSString *> *g_pendingTracked;  // track requests for names not yet seen
static NSMutableDictionary<NSString *, NSArray<NSNumber *> *> *g_shapeHints;  // name → [rows, cols]
static os_unfair_lock g_bufLock = OS_UNFAIR_LOCK_INIT;
static atomic_int g_bufCounter;
static atomic_int g_step;
static atomic_int g_maxDim;
static BOOL g_trackAll = NO;

// Re-entrancy guard: buffers/queues the probe itself creates for staging must
// not be registered or we'd recurse forever.
static pthread_key_t g_inProbeKey;
static BOOL in_probe(void) { return pthread_getspecific(g_inProbeKey) != NULL; }
static void set_in_probe(BOOL v) { pthread_setspecific(g_inProbeKey, v ? (void *)1 : NULL); }

static const char *storage_name(MTLStorageMode m) {
  switch (m) {
    case MTLStorageModeShared: return "shared";
    case MTLStorageModePrivate: return "private";
    case MTLStorageModeManaged: return "managed";
    case MTLStorageModeMemoryless: return "memoryless";
  }
  return "unknown";
}

static void hook_buffer_label_class(Class bufClass);

// Allocation-site capture. Unlabeled buffers get a fallback name from the
// first app-image symbol in the backtrace, and the decl carries the raw
// app-frame return addresses (innermost first) plus the executable path and
// load address so the IDE can resolve real variable names via atos + source.
static const char *main_exe_path(void) {
  static char path[1024];
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    uint32_t size = sizeof(path);
    if (_NSGetExecutablePath(path, &size) != 0) path[0] = '\0';
  });
  return path[0] ? path : NULL;
}

// Load address of the main executable image. NOT _dyld_get_image_header(0):
// with DYLD_INSERT_LIBRARIES (how this probe is injected) the inserted dylib
// can occupy index 0 — find the image by path instead.
static const struct mach_header *main_exe_header(void) {
  static const struct mach_header *hdr;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    const char *exe = main_exe_path();
    for (uint32_t i = 0; exe && i < _dyld_image_count(); i++) {
      const char *nm = _dyld_get_image_name(i);
      if (nm && strcmp(nm, exe) == 0) {
        hdr = _dyld_get_image_header(i);
        return;
      }
    }
    hdr = _dyld_get_image_header(0);
  });
  return hdr;
}

static NSArray<NSString *> *app_frame_addrs(NSString **symOut) {
  const char *exe = main_exe_path();
  if (!exe) return @[];
  void *frames[12];
  int n = backtrace(frames, 12);
  NSMutableArray<NSString *> *out = [NSMutableArray new];
  for (int i = 2; i < n && out.count < 6; i++) {
    Dl_info dli;
    if (!dladdr(frames[i], &dli) || !dli.dli_fname) continue;
    if (strcmp(dli.dli_fname, exe) != 0) continue;  // app frames only
    [out addObject:[NSString stringWithFormat:@"0x%llx", (unsigned long long)frames[i]]];
    if (symOut && !*symOut && dli.dli_sname) {
      int status = 0;
      char *dem = abi::__cxa_demangle(dli.dli_sname, NULL, NULL, &status);
      NSString *sym = [NSString stringWithUTF8String:(status == 0 && dem) ? dem : dli.dli_sname];
      if (dem) free(dem);
      NSRange paren = [sym rangeOfString:@"("];
      if (paren.location != NSNotFound) sym = [sym substringToIndex:paren.location];
      if (sym.length > 48) sym = [sym substringToIndex:48];
      if (sym.length) *symOut = sym;
    }
  }
  return out;
}

static void register_buffer(id<MTLBuffer> buf) {
  if (!buf || in_probe()) return;
  static dispatch_once_t once;
  dispatch_once(&once, ^{ hook_buffer_label_class(object_getClass(buf)); });
  int idx = atomic_fetch_add(&g_bufCounter, 1);
  JadeBufInfo *info = [JadeBufInfo new];
  info.buffer = buf;
  NSString *sym = nil;
  NSArray<NSString *> *addrs = buf.label ? @[] : app_frame_addrs(&sym);
  info.name = buf.label ?: (sym ? [NSString stringWithFormat:@"%@ #%d", sym, idx]
                                : [NSString stringWithFormat:@"buffer#%d", idx]);
  info.length = buf.length;
  info.storage = buf.storageMode;
  os_unfair_lock_lock(&g_bufLock);
  info.tracked = g_trackAll || [g_pendingTracked containsObject:info.name];
  if (info.tracked) info.pinned = buf;
  NSArray<NSNumber *> *hint = g_shapeHints[info.name];
  if (hint) {
    info.shapeRows = hint[0].unsignedIntegerValue;
    info.shapeCols = hint[1].unsignedIntegerValue;
  }
  [g_buffers addObject:info];
  os_unfair_lock_unlock(&g_bufLock);

  NSMutableDictionary *meta = [@{
    @"bytes" : @(info.length),
    @"storage" : @(storage_name(info.storage)),
  } mutableCopy];
  const char *exe = main_exe_path();
  if (addrs.count && exe) {
    meta[@"addrs"] = addrs;  // innermost app frame first
    meta[@"exe"] = @(exe);
    meta[@"load"] = [NSString stringWithFormat:@"0x%llx",
                     (unsigned long long)main_exe_header()];
  }
  emit_json(@{
    @"type" : @"decl",
    @"kind" : @"buffer",
    @"name" : info.name,
    @"meta" : meta,
  });
}

// ─── readback ────────────────────────────────────────────────────────────────

static id<MTLCommandQueue> g_probeQueue;   // our own queue, for staging blits
static id<MTLDevice> g_device;

// Copy a private buffer's bytes to CPU via blit → shared staging. Called from a
// command-buffer completion handler, so the app's GPU work on it is done.
static NSData *read_buffer_bytes(id<MTLBuffer> buf) {
  if (buf.storageMode == MTLStorageModeShared || buf.storageMode == MTLStorageModeManaged) {
    return [NSData dataWithBytes:buf.contents length:buf.length];
  }
  if (buf.storageMode != MTLStorageModePrivate) return nil;  // memoryless: not readable
  set_in_probe(YES);
  if (!g_probeQueue) g_probeQueue = [g_device newCommandQueue];
  id<MTLBuffer> staging = [g_device newBufferWithLength:buf.length
                                                options:MTLResourceStorageModeShared];
  id<MTLCommandBuffer> cb = [g_probeQueue commandBuffer];
  id<MTLBlitCommandEncoder> blit = [cb blitCommandEncoder];
  [blit copyFromBuffer:buf sourceOffset:0 toBuffer:staging destinationOffset:0 size:buf.length];
  [blit endEncoding];
  [cb commit];
  [cb waitUntilCompleted];
  set_in_probe(NO);
  return [NSData dataWithBytes:staging.contents length:staging.length];
}

// ─── dtype detection ─────────────────────────────────────────────────────────
// A raw MTLBuffer doesn't know its element type; ML code uses f32, f16 (half),
// and bf16 (bfloat) interchangeably, and decoding with the wrong one produces
// garbage (solid-color frames). Score each interpretation and pick the best.

typedef enum { JADE_DT_F32 = 0, JADE_DT_F16 = 1, JADE_DT_BF16 = 2 } JadeDtype;
static const char *dtype_name(JadeDtype d) {
  return d == JADE_DT_F16 ? "f16" : d == JADE_DT_BF16 ? "bf16" : "f32";
}

static inline float bf16_to_f32(uint16_t h) {
  uint32_t u = ((uint32_t)h) << 16;
  float f;
  memcpy(&f, &u, 4);
  return f;
}

static inline float read_as(JadeDtype dt, const void *bytes, NSUInteger i) {
  switch (dt) {
    case JADE_DT_F32: return ((const float *)bytes)[i];
    case JADE_DT_F16: return (float)((const __fp16 *)bytes)[i];
    case JADE_DT_BF16: return bf16_to_f32(((const uint16_t *)bytes)[i]);
  }
  return 0;
}

static inline BOOL sane_weight(float x) {
  if (!isfinite(x)) return NO;
  if (x == 0.0f) return YES;
  float a = fabsf(x);
  return a > 1e-6f && a < 1e6f;
}

static int cmp_float(const void *a, const void *b) {
  float x = *(const float *)a, y = *(const float *)b;
  return x < y ? -1 : x > y ? 1 : 0;
}

// Score: fraction of sampled values that look weight-like, penalized when the
// nonzero magnitudes cluster in a narrow band (misreads collapse real weights
// into tight bands — e.g. bf16 data as f16 lands in [1,2)).
static double interp_score(NSData *raw, JadeDtype dt) {
  NSUInteger elemSize = (dt == JADE_DT_F32) ? 4 : 2;
  NSUInteger n = raw.length / elemSize;
  if (n == 0) return 0;
  NSUInteger stride = MAX((NSUInteger)1, n / 1024);
  float mags[1200];
  NSUInteger sane = 0, total = 0, nz = 0;
  for (NSUInteger i = 0; i < n && nz < 1200; i += stride, total++) {
    float x = read_as(dt, raw.bytes, i);
    if (sane_weight(x)) {
      sane++;
      if (x != 0.0f) mags[nz++] = fabsf(x);
    }
  }
  if (total == 0) return 0;
  double score = (double)sane / (double)total;
  if (nz >= 32) {
    qsort(mags, nz, sizeof(float), cmp_float);
    float p50 = mags[nz / 2], p90 = mags[(nz * 9) / 10];
    if (p50 > 0 && p90 / p50 < 1.3f) score *= 0.6;  // suspicious clustering
  }
  return score;
}

// Mean mantissa density (set bits / mantissa width) — tiebreaker between f16
// and bf16 for near-constant data: the correct read of round values (1.0,
// 384, …) has few mantissa bits set, while the misread smears the exponent
// into the mantissa field.
static double mantissa_density(NSData *raw, JadeDtype dt) {
  NSUInteger n = raw.length / 2;
  if (n == 0) return 1;
  const uint16_t *h = (const uint16_t *)raw.bytes;
  const int mantBits = (dt == JADE_DT_F16) ? 10 : 7;
  const uint16_t mask = (uint16_t)((1u << mantBits) - 1);
  NSUInteger stride = MAX((NSUInteger)1, n / 1024);
  NSUInteger bits = 0, total = 0;
  for (NSUInteger i = 0; i < n; i += stride, total++) {
    bits += __builtin_popcount(h[i] & mask);
  }
  return total ? (double)bits / (double)(total * mantBits) : 1;
}

// Detect the element type of a raw buffer.
//
// f32 first, via structure rather than value plausibility: in little-endian
// f32 data the odd 16-bit halves carry sign+exponent (decode sane as bf16)
// while the even halves are mantissa noise (mostly insane or exact zero).
// True 16-bit data has no such even/odd asymmetry. Then f16 vs bf16 by
// interpretation score, tie-broken by which puts magnitudes nearest ~1.
static JadeDtype detect_dtype(NSData *raw) {
  NSUInteger pairs = raw.length / 4;
  if (pairs < 8) return JADE_DT_F32;
  const uint16_t *h = (const uint16_t *)raw.bytes;
  NSUInteger stride = MAX((NSUInteger)1, pairs / 1024);
  NSUInteger evenSane = 0, evenZero = 0, oddSane = 0, oddNzSane = 0, total = 0;
  for (NSUInteger p = 0; p < pairs; p += stride, total++) {
    float lo = bf16_to_f32(h[2 * p]);      // f32 mantissa half
    float hi = bf16_to_f32(h[2 * p + 1]);  // f32 sign+exponent half
    if (sane_weight(lo)) {
      evenSane++;
      if (lo == 0.0f) evenZero++;
    }
    if (sane_weight(hi)) {
      oddSane++;
      if (hi != 0.0f) oddNzSane++;
    }
  }
  if (total == 0) return JADE_DT_F32;
  const double evenS = (double)evenSane / total, oddS = (double)oddSane / total;
  const double evenZ = (double)evenZero / total, oddNz = (double)oddNzSane / total;
  if (oddS - evenS > 0.3) return JADE_DT_F32;          // mantissa halves are noise
  if (evenZ > 0.5 && oddNz > 0.5) return JADE_DT_F32;  // round f32 constants

  const double sf16 = interp_score(raw, JADE_DT_F16);
  const double sbf16 = interp_score(raw, JADE_DT_BF16);
  if (sbf16 > sf16 + 0.05) return JADE_DT_BF16;
  if (sf16 > sbf16 + 0.05) return JADE_DT_F16;
  return mantissa_density(raw, JADE_DT_BF16) <= mantissa_density(raw, JADE_DT_F16)
             ? JADE_DT_BF16
             : JADE_DT_F16;
}

// Downsample float data to <=maxDim x maxDim (average pooling) and emit a
// tensor frame. Shape AND dtype are unknowable from a raw MTLBuffer: we guess
// the largest square that fits and pick f32 / f16 / bf16 by interpretation
// score (see interp_score above).
static void emit_tensor_frame(JadeBufInfo *info, int step, int maxDim) {
  id<MTLBuffer> buf = info.pinned ?: info.buffer;
  if (!buf) return;
  NSData *raw = read_buffer_bytes(buf);
  if (!raw) return;

  JadeDtype dt = detect_dtype(raw);

  // A user shape hint that doesn't fit the detected element width but fits a
  // 16-bit read is strong evidence the width detection was wrong — the user
  // knows their tensor. Pick the better-scoring 16-bit flavor.
  const NSUInteger hintR = info.shapeRows, hintC = info.shapeCols;
  if (hintR > 0 && hintC > 0 && dt == JADE_DT_F32 &&
      hintR * hintC > raw.length / 4 && hintR * hintC <= raw.length / 2) {
    dt = interp_score(raw, JADE_DT_F16) >= interp_score(raw, JADE_DT_BF16)
             ? JADE_DT_F16
             : JADE_DT_BF16;
  }

  NSUInteger n = raw.length / (dt == JADE_DT_F32 ? 4 : 2);
  if (n == 0) return;
  // Normalize to a float32 view (converted copy for 16-bit types).
  NSMutableData *converted = nil;
  const float *src;
  if (dt == JADE_DT_F32) {
    src = (const float *)raw.bytes;
  } else {
    converted = [NSMutableData dataWithLength:n * sizeof(float)];
    float *dst = (float *)converted.mutableBytes;
    for (NSUInteger i = 0; i < n; i++) dst[i] = read_as(dt, raw.bytes, i);
    src = dst;
  }
  NSUInteger rows, cols;
  if (hintR > 0 && hintC > 0 && hintR * hintC <= n) {
    rows = hintR;
    cols = hintC;
  } else {
    NSUInteger side = (NSUInteger)floor(sqrt((double)n));
    if (side == 0) return;
    rows = side;
    cols = side;
  }
  // Aspect-preserving downsample: cap the LARGER dimension at maxDim and
  // scale the other proportionally, so a 256×384 tensor pools to 85×128
  // (not 128×128) and renders with its true aspect ratio downstream.
  NSUInteger outR, outC;
  if (rows >= cols) {
    outR = MIN(rows, (NSUInteger)maxDim);
    outC = MIN(cols, MAX((NSUInteger)1, (cols * outR + rows / 2) / rows));
  } else {
    outC = MIN(cols, (NSUInteger)maxDim);
    outR = MIN(rows, MAX((NSUInteger)1, (rows * outC + cols / 2) / cols));
  }
  NSMutableData *out = [NSMutableData dataWithLength:outR * outC * sizeof(float)];
  float *dst = (float *)out.mutableBytes;
  for (NSUInteger r = 0; r < outR; r++) {
    NSUInteger r0 = r * rows / outR, r1 = MAX(r0 + 1, (r + 1) * rows / outR);
    for (NSUInteger c = 0; c < outC; c++) {
      NSUInteger c0 = c * cols / outC, c1 = MAX(c0 + 1, (c + 1) * cols / outC);
      double acc = 0;
      for (NSUInteger rr = r0; rr < r1; rr++)
        for (NSUInteger cc = c0; cc < c1; cc++) {
          float v = src[rr * cols + cc];
          if (isfinite(v)) acc += v;  // NaN/Inf cells count as 0
        }
      dst[r * outC + c] = (float)(acc / ((r1 - r0) * (c1 - c0)));
    }
  }
  emit_json(@{
    @"type" : @"tensor",
    @"name" : info.name,
    @"step" : @(step),
    @"rows" : @(outR),
    @"cols" : @(outC),
    // Pre-pooling dimensions, so the IDE can label axes with real indices.
    @"srcRows" : @(rows),
    @"srcCols" : @(cols),
    // Payload is always f32; dtype records the detected source element type.
    @"dtype" : @(dtype_name(dt)),
    @"data" : [out base64EncodedStringWithOptions:0],
  });
}

// ─── swizzling machinery ─────────────────────────────────────────────────────

static IMP swizzle(Class cls, SEL sel, id block) {
  Method m = class_getInstanceMethod(cls, sel);
  if (!m) return NULL;
  IMP newImp = imp_implementationWithBlock(block);
  // Add to the concrete class first in case the method lives on a superclass.
  if (class_addMethod(cls, sel, newImp, method_getTypeEncoding(m))) {
    return method_getImplementation(m);
  }
  IMP orig = method_getImplementation(m);
  method_setImplementation(m, newImp);
  return orig;
}

// ─── pipeline → kernel-name tracking ─────────────────────────────────────────
// Per-command-buffer timings are only useful if they distinguish work. Track
// which compute pipelines each command buffer dispatches: PSO-creation hooks
// record pipeline → MTLFunction.name, encoder hooks accumulate the names on
// the command buffer, and the completion timing is emitted under them
// (e.g. "computeQKV" instead of "gpu.commandBuffer").

static NSMapTable<id, NSString *> *g_psoNames;  // PSO (weak) → kernel name
static os_unfair_lock g_psoLock = OS_UNFAIR_LOCK_INIT;
static void *kKernelSetKey = &kKernelSetKey;

static void record_pso_name(id pso, NSString *name) {
  if (!pso || !name.length) return;
  os_unfair_lock_lock(&g_psoLock);
  [g_psoNames setObject:name forKey:pso];
  os_unfair_lock_unlock(&g_psoLock);
}

static NSString *pso_name(id<MTLComputePipelineState> pso) {
  if (!pso) return nil;
  if ([pso respondsToSelector:@selector(label)] && pso.label.length) return pso.label;
  os_unfair_lock_lock(&g_psoLock);
  NSString *name = [g_psoNames objectForKey:pso];
  os_unfair_lock_unlock(&g_psoLock);
  return name;
}

static void note_encoder_kernel(id encoder, NSString *kernel);  // per-encoder timing (below)

static void hook_compute_encoder_class(Class encClass) {
  static void *kHooked = &kHooked;
  if (objc_getAssociatedObject(encClass, kHooked)) return;
  objc_setAssociatedObject(encClass, kHooked, @YES, OBJC_ASSOCIATION_RETAIN);

  static IMP origSetPso;
  origSetPso = swizzle(
      encClass, @selector(setComputePipelineState:),
      ^(id<MTLComputeCommandEncoder> self, id<MTLComputePipelineState> pso) {
        ((void (*)(id, SEL, id))origSetPso)(self, @selector(setComputePipelineState:), pso);
        NSMutableSet *kernels = objc_getAssociatedObject(self, kKernelSetKey);
        NSString *name = pso_name(pso);
        if (kernels && name) {
          os_unfair_lock_lock(&g_psoLock);
          [kernels addObject:name];
          os_unfair_lock_unlock(&g_psoLock);
        }
        note_encoder_kernel(self, name);  // per-encoder timing series
      });
}

// Share one kernel-name set between a command buffer and its encoders.
static void attach_kernel_set(id<MTLCommandBuffer> cb, id encoder) {
  if (!cb || !encoder) return;
  NSMutableSet *set = objc_getAssociatedObject(cb, kKernelSetKey);
  if (!set) {
    set = [NSMutableSet new];
    objc_setAssociatedObject(cb, kKernelSetKey, set, OBJC_ASSOCIATION_RETAIN);
  }
  objc_setAssociatedObject(encoder, kKernelSetKey, set, OBJC_ASSOCIATION_RETAIN);
  hook_compute_encoder_class(object_getClass(encoder));
}

// Timing name for a completed command buffer: explicit label > kernel names.
// Long joins keep a readable prefix plus an FNV-1a hash of the FULL join: a
// bare prefix truncation collapsed every kernel set sharing the first 61
// chars into one series, and the cut point shifted whenever the set changed.
static NSString *command_buffer_timing_name(id<MTLCommandBuffer> cb) {
  if (cb.label.length) return cb.label;
  NSSet<NSString *> *kernels = objc_getAssociatedObject(cb, kKernelSetKey);
  if (kernels.count) {
    NSArray *sorted = [kernels.allObjects sortedArrayUsingSelector:@selector(compare:)];
    NSString *joined = [sorted componentsJoinedByString:@"+"];
    if (joined.length > 64) {
      uint32_t h = 2166136261u; // FNV-1a over the untruncated join
      for (NSUInteger i = 0; i < joined.length; i++)
        h = (h ^ (uint32_t)[joined characterAtIndex:i]) * 16777619u;
      joined = [NSString stringWithFormat:@"%@…%08x", [joined substringToIndex:52], h];
    }
    return joined;
  }
  return @"gpu.commandBuffer";
}

// ─── timer tracking: samples are opt-in, names are declared on discovery ─────
// Timers default to NOT tracked: a training loop emits hundreds of samples/s
// across many kernel names, which floods the IDE when nobody is looking. Each
// new timing name is declared once (decl kind=timer) so the IDE's pre-run
// panel can list it; samples flow only after {"type":"track","kind":"timer"}.

static NSMutableSet<NSString *> *g_trackedTimers;
static NSMutableSet<NSString *> *g_declaredTimers;
static os_unfair_lock g_timerLock = OS_UNFAIR_LOCK_INIT;

// Declares `name` on first sight (returns outside the lock before emitting —
// emit_json can block on the socket). Returns whether samples should be sent.
static bool note_timer_name(NSString *name) {
  bool declare = false, tracked = false;
  os_unfair_lock_lock(&g_timerLock);
  if (![g_declaredTimers containsObject:name]) {
    [g_declaredTimers addObject:name];
    declare = true;
  }
  tracked = g_trackAll || [g_trackedTimers containsObject:name];
  os_unfair_lock_unlock(&g_timerLock);
  if (declare) {
    emit_json(@{@"type" : @"decl", @"kind" : @"timer", @"name" : name});
  }
  return tracked;
}

static void set_timer_tracked(NSString *name, BOOL enabled) {
  os_unfair_lock_lock(&g_timerLock);
  if (enabled) [g_trackedTimers addObject:name];
  else [g_trackedTimers removeObject:name];
  os_unfair_lock_unlock(&g_timerLock);
}

// ─── per-encoder GPU timing (counter sampling at stage boundaries) ───────────
// GPUStartTime/GPUEndTime only time the WHOLE command buffer, so once an app
// encodes every pipeline stage into one command buffer the timers all merge
// into a single joined series. Apple GPUs support timestamp sampling at
// encoder (stage) boundaries: each compute encoder is created through a
// MTLComputePassDescriptor holding a slice of a shared per-command-buffer
// sample buffer, and completion resolves per-encoder GPU durations, summed
// per kernel name — one sample per kernel per command buffer, the same shape
// the old one-command-buffer-per-pipeline apps produced. Emitted only when
// the command buffer mixes ≥2 kernels (a single-kernel buffer is already
// covered by the whole-buffer timer of the same name).

static id<MTLCounterSet> g_timestampCounterSet;  // nil = unsupported
static const NSUInteger CSB_SAMPLES = 256;       // 128 encoders per command buffer
static os_unfair_lock g_csbPoolLock = OS_UNFAIR_LOCK_INIT;
static NSMutableArray<id<MTLCounterSampleBuffer>> *g_csbPool;

// GPU-tick → nanosecond calibration from two correlated CPU/GPU samples
// (`sampleTimestamps:` returns both clocks at one instant; the ratio of CPU-ns
// elapsed to GPU-ticks elapsed between two calls is the tick length).
static os_unfair_lock g_tsCalLock = OS_UNFAIR_LOCK_INIT;
static MTLTimestamp g_calCpu0, g_calGpu0;
static double g_gpuNsPerTick;

static void refresh_gpu_timebase(id<MTLDevice> dev) {
  if (![dev respondsToSelector:@selector(sampleTimestamps:gpuTimestamp:)]) return;
  MTLTimestamp cpu = 0, gpu = 0;
  [dev sampleTimestamps:&cpu gpuTimestamp:&gpu];
  if (!cpu || !gpu) return;
  os_unfair_lock_lock(&g_tsCalLock);
  if (!g_calCpu0) {
    g_calCpu0 = cpu;
    g_calGpu0 = gpu;
  } else if (gpu > g_calGpu0 && cpu > g_calCpu0) {
    g_gpuNsPerTick = (double)(cpu - g_calCpu0) / (double)(gpu - g_calGpu0);
  }
  os_unfair_lock_unlock(&g_tsCalLock);
}

static double gpu_delta_ms(MTLTimestamp start, MTLTimestamp end) {
  if (end <= start) return 0;
  os_unfair_lock_lock(&g_tsCalLock);
  double scale = g_gpuNsPerTick;
  os_unfair_lock_unlock(&g_tsCalLock);
  if (scale <= 0) return 0;
  return (double)(end - start) * scale / 1e6;
}

// One command buffer's encoder-timing state: the shared sample buffer, and per
// handed-out sample pair the kernel name that ran (samples 2i, 2i+1 bracket
// encoder i). Names are filled by the setComputePipelineState hook.
@interface JadeCBSamples : NSObject
@property(nonatomic, strong) id<MTLCounterSampleBuffer> csb;
@property(nonatomic, strong) NSMutableArray<NSString *> *names;  // "" until a PSO is set
@end
@implementation JadeCBSamples
@end

static void *kCBSamplesKey = &kCBSamplesKey;
static void *kEncSamplesKey = &kEncSamplesKey;
static void *kEncPairKey = &kEncPairKey;

static id<MTLCounterSampleBuffer> acquire_csb(id<MTLDevice> dev) {
  os_unfair_lock_lock(&g_csbPoolLock);
  id<MTLCounterSampleBuffer> csb = g_csbPool.lastObject;
  if (csb) [g_csbPool removeLastObject];
  os_unfair_lock_unlock(&g_csbPoolLock);
  if (csb) return csb;
  MTLCounterSampleBufferDescriptor *d = [MTLCounterSampleBufferDescriptor new];
  d.counterSet = g_timestampCounterSet;
  d.storageMode = MTLStorageModeShared;
  d.sampleCount = CSB_SAMPLES;
  NSError *err = nil;
  return [dev newCounterSampleBufferWithDescriptor:d error:&err];  // nil on failure → unsampled
}

static void release_csb(id<MTLCounterSampleBuffer> csb) {
  if (!csb) return;
  os_unfair_lock_lock(&g_csbPoolLock);
  if (!g_csbPool) g_csbPool = [NSMutableArray new];
  if (g_csbPool.count < 8) [g_csbPool addObject:csb];
  os_unfair_lock_unlock(&g_csbPoolLock);
}

// Create a compute encoder with start/end-of-encoder timestamp samples wired
// in, sharing the command buffer's sample buffer. Falls back to `nil` (caller
// uses the plain encoder path) when sampling is unavailable or exhausted.
static id jade_sampled_encoder(id<MTLCommandBuffer> cb, MTLDispatchType type) {
  if (!g_timestampCounterSet) return nil;
  if (![cb respondsToSelector:@selector(computeCommandEncoderWithDescriptor:)]) return nil;
  JadeCBSamples *s = objc_getAssociatedObject(cb, kCBSamplesKey);
  if (!s) {
    id<MTLCounterSampleBuffer> csb = acquire_csb(cb.device);
    if (!csb) return nil;
    s = [JadeCBSamples new];
    s.csb = csb;
    s.names = [NSMutableArray new];
    objc_setAssociatedObject(cb, kCBSamplesKey, s, OBJC_ASSOCIATION_RETAIN);
  }
  NSUInteger pair = s.names.count;
  if (pair * 2 + 1 >= CSB_SAMPLES) return nil;  // slice exhausted → unsampled encoder

  MTLComputePassDescriptor *desc = [MTLComputePassDescriptor computePassDescriptor];
  desc.dispatchType = type;
  MTLComputePassSampleBufferAttachmentDescriptor *att = desc.sampleBufferAttachments[0];
  att.sampleBuffer = s.csb;
  att.startOfEncoderSampleIndex = pair * 2;
  att.endOfEncoderSampleIndex = pair * 2 + 1;
  id enc = [cb computeCommandEncoderWithDescriptor:desc];
  if (!enc) return nil;
  [s.names addObject:@""];
  objc_setAssociatedObject(enc, kEncSamplesKey, s, OBJC_ASSOCIATION_RETAIN);
  objc_setAssociatedObject(enc, kEncPairKey, @(pair), OBJC_ASSOCIATION_RETAIN);
  return enc;
}

// Record the kernel that ran in a sampled encoder (called from the
// setComputePipelineState hook). Repeat PSOs in one encoder join with '+'.
static void note_encoder_kernel(id encoder, NSString *kernel) {
  if (!kernel) return;
  JadeCBSamples *s = objc_getAssociatedObject(encoder, kEncSamplesKey);
  NSNumber *pair = objc_getAssociatedObject(encoder, kEncPairKey);
  if (!s || !pair || pair.unsignedIntegerValue >= s.names.count) return;
  NSString *prev = s.names[pair.unsignedIntegerValue];
  if (prev.length == 0) {
    s.names[pair.unsignedIntegerValue] = kernel;
  } else if (![prev isEqualToString:kernel] &&
             ![prev containsString:[NSString stringWithFormat:@"+%@", kernel]]) {
    s.names[pair.unsignedIntegerValue] = [NSString stringWithFormat:@"%@+%@", prev, kernel];
  }
}

// Resolve, aggregate per kernel, emit, and recycle — completion-handler side.
static void emit_encoder_timings(id<MTLCommandBuffer> cb, int step) {
  JadeCBSamples *s = objc_getAssociatedObject(cb, kCBSamplesKey);
  if (!s) return;
  objc_setAssociatedObject(cb, kCBSamplesKey, nil, OBJC_ASSOCIATION_RETAIN);
  refresh_gpu_timebase(cb.device);
  NSUInteger pairs = s.names.count;
  NSData *data = pairs ? [s.csb resolveCounterRange:NSMakeRange(0, pairs * 2)] : nil;
  if (data && data.length >= pairs * 2 * sizeof(MTLCounterResultTimestamp)) {
    const MTLCounterResultTimestamp *ts =
        (const MTLCounterResultTimestamp *)data.bytes;
    NSMutableDictionary<NSString *, NSNumber *> *sums = [NSMutableDictionary new];
    NSMutableArray<NSString *> *order = [NSMutableArray new];
    for (NSUInteger i = 0; i < pairs; i++) {
      NSString *name = s.names[i];
      MTLTimestamp a = ts[i * 2].timestamp, b = ts[i * 2 + 1].timestamp;
      if (name.length == 0 || a == MTLCounterErrorValue || b == MTLCounterErrorValue) continue;
      double ms = gpu_delta_ms(a, b);
      if (ms <= 0) continue;
      NSNumber *acc = sums[name];
      if (!acc) [order addObject:name];
      sums[name] = @(acc.doubleValue + ms);
    }
    // Only worth emitting when the buffer mixes kernels; a single-kernel
    // buffer already gets a whole-buffer timer under this exact name.
    if (sums.count >= 2) {
      for (NSString *name in order) {
        if (note_timer_name(name)) {
          emit_json(@{
            @"type" : @"timing",
            @"name" : name,
            @"ms" : sums[name],
            @"step" : @(step),
          });
        }
      }
    }
  }
  release_csb(s.csb);
}

// ─── command buffer hooks: auto GPU timing + readback scheduling ─────────────

// Buffer streaming policy: snapshots are taken at command-buffer completion
// (the only point where GPU writes are guaranteed visible), rate-limited by a
// token bucket — short programs get a snapshot after EVERY command buffer
// (each is a scrubber position in the 3D view), while sustained training
// loops settle to ~20 batches/s. A trailing-edge flush catches state written
// after the last snapshot, and a destructor flush emits the final state at
// process exit.
static const uint64_t FLUSH_INTERVAL_NS = 50ull * 1000 * 1000;
static const double FLUSH_BURST = 32;  // consecutive per-CB snapshots allowed
static _Atomic bool g_tensorsDirty;
static _Atomic bool g_trailingScheduled;
static dispatch_queue_t g_flushQueue;

static os_unfair_lock g_rateLock = OS_UNFAIR_LOCK_INIT;
static uint64_t g_rateLastRefill;
static double g_rateTokens = FLUSH_BURST;

static bool take_snapshot_token(void) {
  bool ok = false;
  uint64_t now = clock_gettime_nsec_np(CLOCK_MONOTONIC);
  os_unfair_lock_lock(&g_rateLock);
  g_rateTokens += (double)(now - g_rateLastRefill) / (double)FLUSH_INTERVAL_NS;
  if (g_rateTokens > FLUSH_BURST) g_rateTokens = FLUSH_BURST;
  g_rateLastRefill = now;
  if (g_rateTokens >= 1.0) {
    g_rateTokens -= 1.0;
    ok = true;
  }
  os_unfair_lock_unlock(&g_rateLock);
  return ok;
}

static void flush_tracked_buffers(int step) {
  NSArray<JadeBufInfo *> *snapshot;
  os_unfair_lock_lock(&g_bufLock);
  snapshot = [g_buffers copy];
  os_unfair_lock_unlock(&g_bufLock);
  int maxDim = atomic_load(&g_maxDim);
  for (JadeBufInfo *info in snapshot) {
    if (info.tracked) emit_tensor_frame(info, step, maxDim);
  }
}

static void note_gpu_work_done(int step) {
  atomic_store(&g_tensorsDirty, true);

  if (take_snapshot_token()) {
    atomic_store(&g_tensorsDirty, false);
    flush_tracked_buffers(step);
  }

  // Trailing flush: catch buffer states written after the last flush when no
  // further command buffer arrives to trigger one.
  bool expected = false;
  if (g_flushQueue &&
      atomic_compare_exchange_strong(&g_trailingScheduled, &expected, true)) {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(FLUSH_INTERVAL_NS * 2)),
                   g_flushQueue, ^{
                     atomic_store(&g_trailingScheduled, false);
                     if (atomic_exchange(&g_tensorsDirty, false)) {
                       flush_tracked_buffers(atomic_load(&g_step));
                     }
                   });
  }
}

static void hook_command_buffer_class(Class cbClass) {
  static void *kHooked = &kHooked;
  if (objc_getAssociatedObject(cbClass, kHooked)) return;
  objc_setAssociatedObject(cbClass, kHooked, @YES, OBJC_ASSOCIATION_RETAIN);

  // Track which compute kernels get encoded into each command buffer. App
  // encoders are created through a sampled compute pass when the device
  // supports stage-boundary timestamps (per-encoder timing); the plain path
  // is the fallback.
  static IMP origEnc;
  origEnc = swizzle(cbClass, @selector(computeCommandEncoder), ^id(id<MTLCommandBuffer> self) {
    if (!in_probe()) {
      id enc = jade_sampled_encoder(self, MTLDispatchTypeSerial);
      if (!enc) enc = ((id (*)(id, SEL))origEnc)(self, @selector(computeCommandEncoder));
      attach_kernel_set(self, enc);
      return enc;
    }
    return ((id (*)(id, SEL))origEnc)(self, @selector(computeCommandEncoder));
  });
  static IMP origEncDispatch;
  origEncDispatch = swizzle(
      cbClass, @selector(computeCommandEncoderWithDispatchType:),
      ^id(id<MTLCommandBuffer> self, MTLDispatchType type) {
        if (!in_probe()) {
          id enc = jade_sampled_encoder(self, type);
          if (!enc) {
            enc = ((id (*)(id, SEL, MTLDispatchType))origEncDispatch)(
                self, @selector(computeCommandEncoderWithDispatchType:), type);
          }
          attach_kernel_set(self, enc);
          return enc;
        }
        return ((id (*)(id, SEL, MTLDispatchType))origEncDispatch)(
            self, @selector(computeCommandEncoderWithDispatchType:), type);
      });

  static IMP origCommit;
  origCommit = swizzle(cbClass, @selector(commit), ^(id<MTLCommandBuffer> self) {
    if (!in_probe()) {
      [self addCompletedHandler:^(id<MTLCommandBuffer> cb) {
        int step = atomic_fetch_add(&g_step, 1);
        double ms = (cb.GPUEndTime - cb.GPUStartTime) * 1000.0;
        if (ms > 0) {
          NSString *name = command_buffer_timing_name(cb);
          // Declared on first sight; samples only when the IDE tracks it.
          if (note_timer_name(name)) {
            emit_json(@{
              @"type" : @"timing",
              @"name" : name,
              @"ms" : @(ms),
              @"step" : @(step),
            });
          }
        }
        // Per-kernel series from the encoder timestamp samples — each
        // pipeline stays individually selectable even when the app encodes
        // everything into one command buffer.
        emit_encoder_timings(cb, step);
        note_gpu_work_done(step);
      }];
    }
    ((void (*)(id, SEL))origCommit)(self, @selector(commit));
  });
}

// ─── queue + device hooks: buffer discovery ──────────────────────────────────

static void hook_queue_class(Class qClass) {
  static void *kHooked = &kHooked;
  if (objc_getAssociatedObject(qClass, kHooked)) return;
  objc_setAssociatedObject(qClass, kHooked, @YES, OBJC_ASSOCIATION_RETAIN);

  static IMP origCmdBuf;
  origCmdBuf = swizzle(qClass, @selector(commandBuffer), ^id(id<MTLCommandQueue> self) {
    id<MTLCommandBuffer> cb =
        ((id (*)(id, SEL))origCmdBuf)(self, @selector(commandBuffer));
    if (cb) hook_command_buffer_class(object_getClass(cb));
    return cb;
  });
}

static void hook_device(id<MTLDevice> dev) {
  if (!dev) return;
  g_device = dev;

  // Per-encoder timing support: a timestamp counter set sampled at encoder
  // (stage) boundaries — the granularity Apple GPUs support. Absent → the
  // whole-command-buffer timer is the only series (previous behavior).
  if (!g_timestampCounterSet &&
      [dev respondsToSelector:@selector(supportsCounterSampling:)] &&
      [dev supportsCounterSampling:MTLCounterSamplingPointAtStageBoundary]) {
    for (id<MTLCounterSet> cs in dev.counterSets) {
      if ([cs.name isEqualToString:MTLCommonCounterSetTimestamp]) {
        g_timestampCounterSet = cs;
        break;
      }
    }
    refresh_gpu_timebase(dev);  // first calibration point
  }

  Class devClass = object_getClass(dev);
  static void *kHooked = &kHooked;
  if (objc_getAssociatedObject(devClass, kHooked)) return;
  objc_setAssociatedObject(devClass, kHooked, @YES, OBJC_ASSOCIATION_RETAIN);

  static IMP origNewLen;
  origNewLen = swizzle(
      devClass, @selector(newBufferWithLength:options:),
      ^id(id<MTLDevice> self, NSUInteger len, MTLResourceOptions opts) {
        id<MTLBuffer> buf = ((id (*)(id, SEL, NSUInteger, MTLResourceOptions))origNewLen)(
            self, @selector(newBufferWithLength:options:), len, opts);
        register_buffer(buf);
        return buf;
      });

  static IMP origNewBytes;
  origNewBytes = swizzle(
      devClass, @selector(newBufferWithBytes:length:options:),
      ^id(id<MTLDevice> self, const void *bytes, NSUInteger len, MTLResourceOptions opts) {
        id<MTLBuffer> buf =
            ((id (*)(id, SEL, const void *, NSUInteger, MTLResourceOptions))origNewBytes)(
                self, @selector(newBufferWithBytes:length:options:), bytes, len, opts);
        register_buffer(buf);
        return buf;
      });

  static IMP origNewQueue;
  origNewQueue = swizzle(devClass, @selector(newCommandQueue), ^id(id<MTLDevice> self) {
    id<MTLCommandQueue> q = ((id (*)(id, SEL))origNewQueue)(self, @selector(newCommandQueue));
    if (q) hook_queue_class(object_getClass(q));
    return q;
  });

  // Record compute pipeline → kernel function name for per-kernel GPU timings.
  static IMP origPsoFn;
  origPsoFn = swizzle(
      devClass, @selector(newComputePipelineStateWithFunction:error:),
      ^id(id<MTLDevice> self, id<MTLFunction> fn, NSError **error) {
        id pso = ((id (*)(id, SEL, id, NSError **))origPsoFn)(
            self, @selector(newComputePipelineStateWithFunction:error:), fn, error);
        record_pso_name(pso, fn.name);
        return pso;
      });
  static IMP origPsoFnOpts;
  origPsoFnOpts = swizzle(
      devClass, @selector(newComputePipelineStateWithFunction:options:reflection:error:),
      ^id(id<MTLDevice> self, id<MTLFunction> fn, MTLPipelineOption opts, id *refl,
          NSError **error) {
        id pso = ((id (*)(id, SEL, id, MTLPipelineOption, id *, NSError **))origPsoFnOpts)(
            self, @selector(newComputePipelineStateWithFunction:options:reflection:error:), fn,
            opts, refl, error);
        record_pso_name(pso, fn.name);
        return pso;
      });
  static IMP origPsoDesc;
  origPsoDesc = swizzle(
      devClass, @selector(newComputePipelineStateWithDescriptor:options:reflection:error:),
      ^id(id<MTLDevice> self, MTLComputePipelineDescriptor *desc, MTLPipelineOption opts,
          id *refl, NSError **error) {
        id pso = ((id (*)(id, SEL, id, MTLPipelineOption, id *, NSError **))origPsoDesc)(
            self, @selector(newComputePipelineStateWithDescriptor:options:reflection:error:),
            desc, opts, refl, error);
        record_pso_name(pso, desc.label ?: desc.computeFunction.name);
        return pso;
      });
}

// ─── label tracking: buffers are usually named after creation ────────────────

static void hook_buffer_label_class(Class bufClass) {
  static IMP origSetLabel;
  origSetLabel = swizzle(bufClass, @selector(setLabel:), ^(id<MTLBuffer> self, NSString *label) {
    ((void (*)(id, SEL, NSString *))origSetLabel)(self, @selector(setLabel:), label);
    if (in_probe() || !label.length) return;
    os_unfair_lock_lock(&g_bufLock);
    for (JadeBufInfo *info in g_buffers) {
      if (info.buffer == self) {
        NSString *old = info.name;
        info.name = label;
        if ([g_pendingTracked containsObject:label]) info.tracked = YES;
        os_unfair_lock_unlock(&g_bufLock);
        emit_json(@{
          @"type" : @"decl",
          @"kind" : @"buffer",
          @"name" : label,
          @"meta" : @{@"renamedFrom" : old, @"bytes" : @(info.length),
                      @"storage" : @(storage_name(info.storage))},
        });
        return;
      }
    }
    os_unfair_lock_unlock(&g_bufLock);
  });
}

// ─── IDE → probe control channel ─────────────────────────────────────────────
// Reads NDJSON from the telemetry socket. Handles:
//   {"type":"track","kind":"buffer","name":"...","enabled":bool,"maxDim":N}
//   {"type":"track","kind":"timer","name":"...","enabled":bool}
// Scalars are always emitted (cheap, explicit in user code); timing samples
// are opt-in per name (see note_timer_name); the IDE filters display.

static void handle_control_line(NSData *lineData) {
  NSDictionary *msg = [NSJSONSerialization JSONObjectWithData:lineData options:0 error:nil];
  if (![msg isKindOfClass:NSDictionary.class]) return;
  if (![msg[@"type"] isEqual:@"track"]) return;
  if ([msg[@"kind"] isEqual:@"timer"]) {
    NSString *tname = msg[@"name"];
    if ([tname isKindOfClass:NSString.class]) {
      set_timer_tracked(tname, [msg[@"enabled"] boolValue]);
    }
    return;
  }
  if (![msg[@"kind"] isEqual:@"buffer"]) return;
  NSString *name = msg[@"name"];
  BOOL enabled = [msg[@"enabled"] boolValue];
  if (!name) return;
  NSNumber *maxDim = msg[@"maxDim"];
  BOOL maxDimChanged = NO;
  if (maxDim) {
    // 512 cap ≈ 1MB/frame of f32 — fine under the snapshot rate limiter, and
    // lets a ≤512-wide tensor stream UNPOOLED for artifact-free inspection.
    int newDim = MAX(4, MIN(512, maxDim.intValue));
    maxDimChanged = atomic_exchange(&g_maxDim, newDim) != newDim;
  }
  // Optional shape hint: the true tensor dimensions of this buffer.
  NSNumber *rowsNum = msg[@"rows"], *colsNum = msg[@"cols"];
  NSArray<NSNumber *> *shape = nil;
  if ([rowsNum isKindOfClass:NSNumber.class] && [colsNum isKindOfClass:NSNumber.class] &&
      rowsNum.unsignedIntegerValue > 0 && colsNum.unsignedIntegerValue > 0) {
    shape = @[ rowsNum, colsNum ];
  }
  NSMutableArray<JadeBufInfo *> *toEmit = [NSMutableArray new];
  os_unfair_lock_lock(&g_bufLock);
  if (enabled) [g_pendingTracked addObject:name];
  else [g_pendingTracked removeObject:name];
  if (shape) g_shapeHints[name] = shape;
  for (JadeBufInfo *info in g_buffers) {
    if ([info.name isEqualToString:name]) {
      BOOL shapeChanged =
          shape && (info.shapeRows != shape[0].unsignedIntegerValue ||
                    info.shapeCols != shape[1].unsignedIntegerValue);
      if (shape) {
        info.shapeRows = shape[0].unsignedIntegerValue;
        info.shapeCols = shape[1].unsignedIntegerValue;
      }
      if (enabled && (!info.tracked || shapeChanged || maxDimChanged)) [toEmit addObject:info];
      info.tracked = enabled;
      info.pinned = enabled ? (info.pinned ?: info.buffer) : nil;
    }
  }
  os_unfair_lock_unlock(&g_bufLock);
  // Snapshot immediately on enable or reshape — don't make the user wait for
  // the next command buffer to see current contents.
  int step = atomic_load(&g_step);
  int maxD = atomic_load(&g_maxDim);
  for (JadeBufInfo *info in toEmit) emit_tensor_frame(info, step, maxD);
}

static void *control_reader(void *arg) {
  int fd = (int)(intptr_t)arg;
  NSMutableData *acc = [NSMutableData new];
  char buf[4096];
  ssize_t got;
  while ((got = read(fd, buf, sizeof(buf))) > 0) {
    [acc appendBytes:buf length:(NSUInteger)got];
    for (;;) {
      const char *bytes = (const char *)acc.bytes;
      const char *nl = (const char *)memchr(bytes, '\n', acc.length);
      if (!nl) break;
      NSUInteger lineLen = (NSUInteger)(nl - bytes);
      if (lineLen > 0) handle_control_line([acc subdataWithRange:NSMakeRange(0, lineLen)]);
      [acc replaceBytesInRange:NSMakeRange(0, lineLen + 1) withBytes:NULL length:0];
    }
  }
  return NULL;
}

// ─── dyld interposition of the device entry points ───────────────────────────

extern "C" id<MTLDevice> MTLCreateSystemDefaultDevice(void);

static id<MTLDevice> jade_MTLCreateSystemDefaultDevice(void) {
  id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
  hook_device(dev);
  return dev;
}

__attribute__((used)) static struct {
  const void *replacement;
  const void *replacee;
} g_interpose[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)jade_MTLCreateSystemDefaultDevice,
     (const void *)MTLCreateSystemDefaultDevice},
};

// ─── init ────────────────────────────────────────────────────────────────────

__attribute__((constructor)) static void jade_probe_init(void) {
  pthread_key_create(&g_inProbeKey, NULL);
  g_buffers = [NSMutableArray new];
  g_pendingTracked = [NSMutableSet new];
  g_trackedTimers = [NSMutableSet new];
  g_declaredTimers = [NSMutableSet new];
  g_shapeHints = [NSMutableDictionary new];
  g_psoNames = [NSMapTable weakToStrongObjectsMapTable];
  g_trackAll = getenv("JADE_TRACK_ALL") != NULL;
  atomic_store(&g_maxDim, 64);
  g_flushQueue = dispatch_queue_create("jade.probe.flush", DISPATCH_QUEUE_SERIAL);

  const char *sockPath = getenv("JADE_TELEMETRY_SOCK");
  if (sockPath) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {};
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, sockPath, sizeof(addr.sun_path) - 1);
    if (fd >= 0 && connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0) {
      g_sock = fd;
      pthread_t reader;
      pthread_create(&reader, NULL, control_reader, (void *)(intptr_t)fd);
      pthread_detach(reader);
    } else if (fd >= 0) {
      close(fd);
    }
  }
  emit_json(@{
    @"type" : @"decl",
    @"kind" : @"scalar",
    @"name" : @"probe.attached",
    @"meta" : @{@"pid" : @(getpid())},
  });
}

// Final snapshot at process exit: tracked buffers are pinned (strong refs),
// so their last contents are still readable even after the app released them.
__attribute__((destructor)) static void jade_probe_fini(void) {
  if (atomic_exchange(&g_tensorsDirty, false)) {
    flush_tracked_buffers(atomic_load(&g_step));
  }
}
