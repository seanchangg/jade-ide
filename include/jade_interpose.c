#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <malloc/malloc.h>

// Jade malloc/free interposer for macOS
// Uses DYLD_INTERPOSE (the correct macOS interposition mechanism)

static size_t _jade_total_alloc = 0;
static size_t _jade_total_freed = 0;
static size_t _jade_current_heap = 0;
static size_t _jade_peak_heap = 0;
static int _jade_alloc_count = 0;
static int _jade_free_count = 0;

// DYLD_INTERPOSE macro
#define DYLD_INTERPOSE(_replacement, _original) \
    __attribute__((used)) static struct { \
        const void *replacement; \
        const void *original; \
    } _interpose_##_original __attribute__((section("__DATA,__interpose"))) = { \
        (const void *)(unsigned long)&_replacement, \
        (const void *)(unsigned long)&_original \
    };

// Live heap sampling: the exit summary alone gives the IDE's Memory chart a
// single point, so emit the same __JADE_HEAP_SUMMARY line periodically
// (at most every 100ms, checked on each alloc/free). The timestamp is
// updated BEFORE fprintf so any malloc fprintf does internally re-enters
// the wrappers, sees a fresh emit, and skips — no recursion.
static uint64_t _jade_last_emit_ns = 0;

static void jade_maybe_emit(void) {
    uint64_t now = clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW);
    if (now - _jade_last_emit_ns < 100000000ull)
        return;
    _jade_last_emit_ns = now;
    fprintf(stderr, "__JADE_HEAP_SUMMARY|%zu|%zu|%zu|%zu|%d|%d\n",
            _jade_total_alloc, _jade_total_freed,
            _jade_current_heap, _jade_peak_heap,
            _jade_alloc_count, _jade_free_count);
}

// Wrapper functions — call the real implementation, then track
void *jade_malloc(size_t size) {
    void *ptr = malloc(size);
    if (ptr) {
        size_t real_size = malloc_size(ptr);
        _jade_total_alloc += real_size;
        _jade_current_heap += real_size;
        _jade_alloc_count++;
        if (_jade_current_heap > _jade_peak_heap)
            _jade_peak_heap = _jade_current_heap;
    }
    jade_maybe_emit();
    return ptr;
}

void jade_free(void *ptr) {
    if (ptr) {
        size_t real_size = malloc_size(ptr);
        _jade_total_freed += real_size;
        if (_jade_current_heap >= real_size)
            _jade_current_heap -= real_size;
        _jade_free_count++;
    }
    free(ptr);
    jade_maybe_emit();
}

void *jade_calloc(size_t count, size_t size) {
    void *ptr = calloc(count, size);
    if (ptr) {
        size_t real_size = malloc_size(ptr);
        _jade_total_alloc += real_size;
        _jade_current_heap += real_size;
        _jade_alloc_count++;
        if (_jade_current_heap > _jade_peak_heap)
            _jade_peak_heap = _jade_current_heap;
    }
    jade_maybe_emit();
    return ptr;
}

void *jade_realloc(void *ptr, size_t size) {
    size_t old_size = ptr ? malloc_size(ptr) : 0;
    void *new_ptr = realloc(ptr, size);
    if (new_ptr) {
        size_t new_size = malloc_size(new_ptr);
        _jade_total_alloc += new_size;
        _jade_total_freed += old_size;
        _jade_current_heap = _jade_current_heap - old_size + new_size;
        _jade_alloc_count++;
        if (ptr) _jade_free_count++;
        if (_jade_current_heap > _jade_peak_heap)
            _jade_peak_heap = _jade_current_heap;
    }
    jade_maybe_emit();
    return new_ptr;
}

DYLD_INTERPOSE(jade_malloc, malloc)
DYLD_INTERPOSE(jade_free, free)
DYLD_INTERPOSE(jade_calloc, calloc)
DYLD_INTERPOSE(jade_realloc, realloc)

__attribute__((constructor))
static void jade_init(void) {
    fprintf(stderr, "__JADE_INTERPOSE_ACTIVE\n");
}

__attribute__((destructor))
static void jade_fini(void) {
    fprintf(stderr, "__JADE_HEAP_SUMMARY|%zu|%zu|%zu|%zu|%d|%d\n",
            _jade_total_alloc, _jade_total_freed,
            _jade_current_heap, _jade_peak_heap,
            _jade_alloc_count, _jade_free_count);
}
