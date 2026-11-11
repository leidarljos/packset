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

# ONNX Runtime 1.28 stops when CMake is older than 3.28. Ubuntu 22.04's
# package is 3.22. The CMake 3.28.6 Linux binary needs glibc 2.17, so it
# runs on that release, and it is a build tool: it is not linked into
# packset-embed. The runtime also wants the GNU Compiler Collection at
# 11.1 or newer, which Ubuntu 22.04 has. Compiling this tree on 22.04 is
# what keeps the encoder at GLIBC_2.35. On a newer glibc the C++ library
# takes newer symbols, which is how packset-embed came to need GLIBC_2.43.
if ! command -v cmake >/dev/null 2>&1; then
  echo "cmake is not on PATH" >&2
  exit 1
fi
cmake_ver=$(cmake --version | awk 'NR==1 { print $3 }')
python3 - "$cmake_ver" <<'PY'
import sys
parts = []
for piece in sys.argv[1].split("."):
    if not piece.isdigit():
        break
    parts.append(int(piece))
if tuple(parts) < (3, 28):
    sys.stderr.write(
        "ONNX Runtime 1.28 needs CMake 3.28 or newer; this cmake is %s.\n"
        "Ubuntu 22.04's package is 3.22. The CMake 3.28.6 Linux binary runs on glibc 2.35.\n"
        % sys.argv[1]
    )
    raise SystemExit(1)
PY
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
# ORT_OSX_ARCH is the macOS slice, arm64 or x86_64.
# Set it when the release target is not the runner.
# Unset builds the host, which is what CI does.
# ONNX Runtime names the aarch64 slice arm64.
if [ "$(uname -s)" = Darwin ] && [ -n "${ORT_OSX_ARCH:-}" ]; then
  set -- "$@" --osx_arch "$ORT_OSX_ARCH"
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
