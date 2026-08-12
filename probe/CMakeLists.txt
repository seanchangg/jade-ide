cmake_minimum_required(VERSION 3.20)
project(jade_probe OBJCXX CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_executable(jade_probe jade_probe.mm)

# Jade IDE instrumentation headers (idetools.h), passed at configure time
if(DEFINED JADE_INCLUDE_DIR)
    target_include_directories(jade_probe PRIVATE ${JADE_INCLUDE_DIR})
endif()

target_link_libraries(jade_probe PRIVATE
        "-framework Metal"
        "-framework MetalPerformanceShaders"
        "-framework Foundation"
        "-framework QuartzCore"
)
