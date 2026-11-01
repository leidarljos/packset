#!/bin/sh
# ONNX Runtime 1.28.0, CPU, shared library, installed under PREFIX.
# Linux and macOS.
# That is the runtime ort 2.0.0-rc.13 publishes as a prebuilt. This script
# builds it from source instead. The encoder then links the result:
#
#   ORT_LIB_PATH=$PREFIX/lib ORT_PREFER_DYNAMIC_LINK=1 cargo build -p packset-embed
#   LD_LIBRARY_PATH=$PREFIX/lib
#
# A distro libonnxruntime 1.24 or newer is used when pkg-config finds it
# and ORT_LIB_PATH is unset.
set -eu
version=1.28.0
prefix=${1:-"$HOME/onnxruntime"}
src=${ORT_SRC:-"$prefix/src"}
mkdir -p "$prefix" "$(dirname "$src")"

# Prefer gcc. A clang c++ that is not pointed at libstdc++ fails the
# cmake compiler check on a machine where that library lives under gcc's
# own directory.
if [ -z "${CC:-}" ] && command -v gcc >/dev/null 2>&1; then
  export CC=gcc
fi
if [ -z "${CXX:-}" ] && command -v g++ >/dev/null 2>&1; then
  export CXX=g++
fi
case $(uname -s) in
  Darwin) build=$src/build/MacOS/Release ;;
  *) build=$src/build/Linux/Release ;;
esac
if [ "$(uname -s)" = Linux ] && [ -z "${LIBRARY_PATH:-}" ] && command -v gcc >/dev/null 2>&1; then
  export LIBRARY_PATH="$(dirname "$(gcc -print-file-name=libstdc++.so)")"
fi

if [ ! -f "$src/build.sh" ]; then
  git clone --recursive --branch "v$version" --depth 1 \
    https://github.com/microsoft/onnxruntime "$src"
fi

cd "$src"
set -- \
  --config Release \
  --build_shared_lib \
  --parallel \
  --skip_tests \
  --compile_no_warning_as_error \
  --skip_submodule_sync \
  --cmake_extra_defines "CMAKE_INSTALL_PREFIX=$prefix" \
  --cmake_extra_defines onnxruntime_BUILD_UNIT_TESTS=OFF
if [ "$(id -u)" -eq 0 ]; then
  set -- "$@" --allow_running_as_root
fi
./build.sh "$@"

cmake --install "$build" --prefix "$prefix"

lib=$(find "$prefix/lib" "$prefix/lib64" \( -name 'libonnxruntime.so' -o -name 'libonnxruntime.dylib' \) 2>/dev/null | head -n 1 || true)
if [ -z "$lib" ]; then
  built=$(find "$build" \( -name 'libonnxruntime.so' -o -name 'libonnxruntime.dylib' \) | head -n 1)
  mkdir -p "$prefix/lib"
  cp -a "$built" "$prefix/lib/"
  # The soname sits beside the linker name when the install step skipped it.
  find "$build" \( -name 'libonnxruntime.so.*' -o -name 'libonnxruntime.*.dylib' \) -exec cp -a {} "$prefix/lib/" \;
  lib=$prefix/lib/$(basename "$built")
fi
echo "onnxruntime_lib=$(dirname "$lib")"
