#include <stdio.h>

#ifdef __cplusplus
extern "C" {
#endif

__attribute__((no_instrument_function))
void __cyg_profile_func_enter(void *func, void *caller) {
    fprintf(stderr, "__JADE_FUNC_ENTER|%p|%p\n", func, caller);
}

__attribute__((no_instrument_function))
void __cyg_profile_func_exit(void *func, void *caller) {
    fprintf(stderr, "__JADE_FUNC_EXIT|%p|%p\n", func, caller);
}

#ifdef __cplusplus
}
#endif
