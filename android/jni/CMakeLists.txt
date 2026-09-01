cmake_minimum_required(VERSION 3.22)
project(zclaw_jni CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# The FFI contract header
include_directories(${CMAKE_SOURCE_DIR}/../../include)

add_library(zclaw_jni SHARED
    zclaw_jni.cpp
)

# libzclaw.so is NOT linked at build time — the JNI wrapper dlopen()s it at
# runtime. The prebuilt Android libzclaw.so is packaged under
# app/src/main/jniLibs/<abi>/ by Gradle.
#
# ⚠️ IMPORTANT: the .so files shipped in harmony/cpp/libs/ are built for
# OHOS (they link libtime_service_ndk.so, an OHOS-only NDK lib) and will not
# load on Android. Build libzclaw for Android from the Rust source
# (gitee.com/ushaw/ulnclaw) with cargo-ndk:
#     cargo ndk -t arm64-v8a build --release

target_link_libraries(zclaw_jni
    log
    dl
)
