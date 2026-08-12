/*
 * idetools.h — Jade IDE instrumentation header
 *
 * Include this in your C++ project to get:
 *   - Memory allocation tracking (heap usage, leak detection)
 *   - Training scalar logging (loss curves, metrics)
 *   - Operation timing (profiling)
 *
 * All output uses a line protocol on stdout that Jade intercepts.
 * Non-instrumentation stdout is passed through normally.
 *
 * Usage:
 *   #include "idetools.h"
 *
 *   // Track allocations automatically (overrides global new/delete):
 *   #define JADE_TRACK_MEMORY
 *   #include "idetools.h"
 *
 *   // Log a scalar value (e.g., loss):
 *   JADE_SCALAR("loss", step, value);
 *
 *   // Time a scope:
 *   { JADE_TIMER("forward_pass", step); ... }
 *
 *   // Manual allocation tagging:
 *   void* p = JADE_ALLOC(malloc(size), size);
 *   JADE_FREE(p, size);
 */

#ifndef IDETOOLS_H
#define IDETOOLS_H

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <chrono>

namespace jade {

// ── Timestamp ──
inline double now_ms() {
    using namespace std::chrono;
    return duration_cast<microseconds>(
        steady_clock::now().time_since_epoch()
    ).count() / 1000.0;
}

// ── Scalar logging ──
// Outputs: __JADE_SCALAR|name|step|value|timestamp
inline void log_scalar(const char* name, int step, double value) {
    std::fprintf(stdout, "__JADE_SCALAR|%s|%d|%.6f|%.3f\n",
                 name, step, value, now_ms());
    std::fflush(stdout);
}

// ── Allocation logging ──
// Outputs: __JADE_ALLOC|pointer|size|file|line|timestamp
inline void log_alloc(void* ptr, size_t size, const char* file, int line) {
    std::fprintf(stdout, "__JADE_ALLOC|%p|%zu|%s|%d|%.3f\n",
                 ptr, size, file, line, now_ms());
    std::fflush(stdout);
}

// Outputs: __JADE_FREE|pointer|size|file|line|timestamp
inline void log_free(void* ptr, size_t size, const char* file, int line) {
    std::fprintf(stdout, "__JADE_FREE|%p|%zu|%s|%d|%.3f\n",
                 ptr, size, file, line, now_ms());
    std::fflush(stdout);
}

// ── Timing ──
// Outputs: __JADE_TIMING|name|duration_ms|step
class ScopeTimer {
public:
    ScopeTimer(const char* name, int step)
        : name_(name), step_(step), start_(std::chrono::steady_clock::now()) {}

    ~ScopeTimer() {
        using namespace std::chrono;
        auto elapsed = duration_cast<microseconds>(
            steady_clock::now() - start_
        ).count() / 1000.0;
        std::fprintf(stdout, "__JADE_TIMING|%s|%.3f|%d\n",
                     name_, elapsed, step_);
        std::fflush(stdout);
    }

private:
    const char* name_;
    int step_;
    std::chrono::steady_clock::time_point start_;
};

} // namespace jade

// ── Convenience macros ──

#define JADE_SCALAR(name, step, value) \
    jade::log_scalar(name, step, value)

#define JADE_TIMER(name, step) \
    jade::ScopeTimer _jade_timer_##__LINE__(name, step)

#define JADE_ALLOC(ptr, size) \
    ([&]() -> decltype(ptr) { \
        auto _p = (ptr); \
        jade::log_alloc((void*)_p, (size), __FILE__, __LINE__); \
        return _p; \
    })()

#define JADE_FREE(ptr, size) \
    do { \
        jade::log_free((void*)(ptr), (size), __FILE__, __LINE__); \
    } while(0)

// ── Optional: automatic new/delete tracking ──
// Define JADE_TRACK_MEMORY before including idetools.h to enable.

#ifdef JADE_TRACK_MEMORY

// Override global operator new
inline void* operator new(size_t size) {
    void* ptr = std::malloc(size);
    if (!ptr) throw std::bad_alloc();
    // We can't get __FILE__/__LINE__ from here, so we use "global" as source
    jade::log_alloc(ptr, size, "global", 0);
    return ptr;
}

inline void* operator new[](size_t size) {
    void* ptr = std::malloc(size);
    if (!ptr) throw std::bad_alloc();
    jade::log_alloc(ptr, size, "global[]", 0);
    return ptr;
}

inline void operator delete(void* ptr) noexcept {
    if (ptr) {
        jade::log_free(ptr, 0, "global", 0);
        std::free(ptr);
    }
}

inline void operator delete[](void* ptr) noexcept {
    if (ptr) {
        jade::log_free(ptr, 0, "global[]", 0);
        std::free(ptr);
    }
}

inline void operator delete(void* ptr, size_t size) noexcept {
    if (ptr) {
        jade::log_free(ptr, size, "global", 0);
        std::free(ptr);
    }
}

inline void operator delete[](void* ptr, size_t size) noexcept {
    if (ptr) {
        jade::log_free(ptr, size, "global[]", 0);
        std::free(ptr);
    }
}

#endif // JADE_TRACK_MEMORY

#endif // IDETOOLS_H
