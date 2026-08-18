cmake_minimum_required(VERSION 3.20)
project(jade_probe OBJCXX CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# The Makefile builds with -O2. Match it when the caller sets no build type.
# The IDE passes -DCMAKE_BUILD_TYPE=Debug, so this default does not fight it.
if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Release CACHE STRING "Build type" FORCE)
endif()

# Both sources need ARC. Without -fobjc-arc the weak properties in
# jade_probe.mm do not compile.
add_compile_options($<$<COMPILE_LANGUAGE:OBJCXX>:-fobjc-arc>)

# jade_probe.mm has no main(). It is the dylib that DYLD_INSERT_LIBRARIES
# injects, so it must be a library target. PREFIX/SUFFIX give the file the
# name jade_probe.dylib that the IDE and mock_server.py expect.
add_library(jade_probe SHARED jade_probe.mm)
set_target_properties(jade_probe PROPERTIES PREFIX "" SUFFIX ".dylib")

# test_train.mm holds main(). It is the uninstrumented program that validates
# the probe.
add_executable(test_train test_train.mm)

# Jade IDE instrumentation headers (idetools.h), passed at configure time
if(DEFINED JADE_INCLUDE_DIR)
    target_include_directories(jade_probe PRIVATE ${JADE_INCLUDE_DIR})
    target_include_directories(test_train PRIVATE ${JADE_INCLUDE_DIR})
endif()

foreach(JADE_TARGET jade_probe test_train)
    target_link_libraries(${JADE_TARGET} PRIVATE
            "-framework Metal"
            "-framework Foundation"
    )
endforeach()
