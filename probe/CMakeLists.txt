cmake_minimum_required(VERSION 3.20)
project(forge_probe OBJCXX CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_executable(forge_probe forge_probe.mm)

# Forge IDE instrumentation headers (idetools.h), passed at configure time
if(DEFINED FORGE_INCLUDE_DIR)
    target_include_directories(forge_probe PRIVATE ${FORGE_INCLUDE_DIR})
endif()

target_link_libraries(forge_probe PRIVATE
        "-framework Metal"
        "-framework MetalPerformanceShaders"
        "-framework Foundation"
        "-framework QuartzCore"
)
