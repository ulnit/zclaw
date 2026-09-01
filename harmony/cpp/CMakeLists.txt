# CMakeLists.txt — ZClaw NAPI module for HarmonyOS
# Builds the C++ NAPI wrapper that loads libzclaw.so

cmake_minimum_required(VERSION 3.16)
project(zclaw_napi)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# OHOS NAPI headers
include_directories(${OHOS_SYSROOT_PATH}/usr/include)
include_directories(${OHOS_SYSROOT_PATH}/usr/include/napi)

# Source files — only the NAPI wrapper, libzclaw.so is prebuilt
# NOTE: The NAPI wrapper is named 'zclaw_napi' to avoid collision with the
#       prebuilt Rust library 'libzclaw.so' which is loaded at runtime via dlopen.
add_library(zclaw_napi SHARED
    zclaw_napi.cpp
)

# libzclaw.so is loaded at runtime via dlopen, not linked at build time.
# OHOS NAPI is provided by libace_napi.z.so in the sysroot.
target_link_libraries(zclaw_napi
    ace_napi.z
    dl
)

# Set output directory for the NAPI wrapper
set_target_properties(zclaw_napi PROPERTIES
    LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib/${OHOS_ARCH}
)

# Copy prebuilt libzclaw.so (the Rust library) to the output directory
# so it can be found by dlopen("libzclaw.so", ...) at runtime.
# Only copy if the prebuilt library exists for the current architecture.
# On x86_64 (emulator), libzclaw.so is not available, so the NAPI wrapper
# will report isAvailable()=false and the app falls back to ArkTS agent loop.
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/libs/${OHOS_ARCH}/libzclaw.so")
    add_custom_command(TARGET zclaw_napi POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E copy
            ${CMAKE_CURRENT_SOURCE_DIR}/libs/${OHOS_ARCH}/libzclaw.so
            ${CMAKE_BINARY_DIR}/lib/${OHOS_ARCH}/libzclaw.so
    )
endif()
