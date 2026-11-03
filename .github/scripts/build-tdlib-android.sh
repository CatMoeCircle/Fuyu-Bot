#!/usr/bin/env bash
# TDLib for **Android**（tdjson）—— 官方 CMake 流程 + NDK 交叉编译。
#
# 与桌面脚本（build-tdlib-linux.sh / -macos.sh）的两处根本差别：
#
# 1. **必须先做一次 host 构建**。TDLib 的生成源码（`td/generate/auto/**`）不在仓库里
#    （`.gitignore` 里有 `**/auto/`），而 `prepare_cross_compiling` 目标被
#    `if (NOT CMAKE_CROSSCOMPILING)` 包着——交叉编译时它根本不存在。所以这里先用
#    主机编译器把生成器跑一遍，把源码落到 `$TDLIB_SRC/td/generate/auto`，再做 Android 构建。
#    ⇒ host 侧需要 gperf + 一个 C++ 编译器（CI 的 apt 装了 g++）。
#
# 2. **Android 没有系统 OpenSSL**。这里自己用 NDK 编译一份 **shared** OpenSSL
#    （`shared`），zlib 用 NDK sysroot 里的 `libz.so`（Android 系统自带，不算额外运行依赖）。
#    ⚠️ 与桌面不同：这里**不**把 OpenSSL / libc++ 静态并进 tdjson，而是把
#    `libssl.so*` / `libcrypto.so*` / `libc++_shared.so` 作为**独立 .so** 一起产出。
#    理由（2026 设备侧）：静态并进去之后，这些库自己 `pthread_key_create` 注册的
#    TLS 析构也跟着进了 tdjson；bionic 不会因为 `dlclose(tdjson)` 撤销那个 key，
#    之后线程退出就在 `pthread_key_clean_all` 里跳到已解除映射的地址。拆成独立
#    文件后，宿主/加载器可以把它们**钉住**（加载一次、永不关闭），key 的
#    destructor 就永远有效。
#
# Args: <tdlib-src-dir> <output-dir> <abi: arm64-v8a | armeabi-v7a | x86 | x86_64>
#
# Env:
#   ANDROID_NDK_HOME / ANDROID_NDK_ROOT   NDK 根目录（必填，二选一）
#   ANDROID_API_LEVEL                     默认 21（TDLib 要求 ≥ 21）
#   OPENSSL_VERSION                       默认 3.5.4（见下面 OPENSSL_SHA256）
#   OPENSSL_CACHE_DIR                     源码/产物缓存目录（默认 $OUT_DIR/openssl-src）
#   OPENSSL_JOBS                          OpenSSL 的 make 并发度，默认 **1**（并行会抢
#                                         `.d.tmp`，见第 2) 步里的说明）
set -euo pipefail

TDLIB_SRC="${1:?tdlib source dir}"
OUT_DIR="${2:?output dir}"
ABI="${3:?abi: arm64-v8a | armeabi-v7a | x86 | x86_64}"

API_LEVEL="${ANDROID_API_LEVEL:-21}"
OPENSSL_VERSION="${OPENSSL_VERSION:-3.5.4}"

# 版本 → ABI 口径（注意 `TRIPLE` 用的是 NDK sysroot 的目录名，和 cargo 的 arch 不是一回事）。
case "$ABI" in
  arm64-v8a)   OPENSSL_TARGET=android-arm64;  TRIPLE=aarch64-linux-android;  CARCH=aarch64 ;;
  armeabi-v7a) OPENSSL_TARGET=android-arm;    TRIPLE=arm-linux-androideabi;  CARCH=arm     ;;
  x86)         OPENSSL_TARGET=android-x86;    TRIPLE=i686-linux-android;     CARCH=x86     ;;
  x86_64)      OPENSSL_TARGET=android-x86_64; TRIPLE=x86_64-linux-android;   CARCH=x86_64  ;;
  *)
    echo "error: unsupported Android ABI: $ABI (支持 arm64-v8a / armeabi-v7a / x86 / x86_64)" >&2
    exit 1
    ;;
esac

NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z "$NDK" || ! -f "$NDK/build/cmake/android.toolchain.cmake" ]]; then
  echo "error: 找不到 Android NDK（ANDROID_NDK_HOME / ANDROID_NDK_ROOT 未指向有效 NDK）" >&2
  echo "       需要 \$NDK/build/cmake/android.toolchain.cmake" >&2
  [[ -n "$NDK" ]] && ls -la "$NDK" >&2 || true
  exit 1
fi
NDK="$(cd "$NDK" && pwd)"

# OpenSSL 的 15-android.conf 认这几个变量名（不同版本认的不一样，索性都导出）。
export ANDROID_NDK_ROOT="$NDK"
export ANDROID_NDK_HOME="$NDK"
export ANDROID_NDK="$NDK"

case "$(uname -s)" in
  Linux)  HOST_TAG=linux-x86_64 ;;
  Darwin) HOST_TAG=darwin-x86_64 ;;
  *)
    echo "error: 只支持在 Linux / macOS runner 上交叉编译 Android（当前 $(uname -s)）" >&2
    exit 1
    ;;
esac
TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG"
SYSROOT="$TOOLCHAIN/sysroot"
if [[ ! -d "$TOOLCHAIN" ]]; then
  echo "error: NDK 里没有 host 工具链 $TOOLCHAIN（Windows 版 NDK 不能用于本脚本）" >&2
  exit 1
fi

NDK_REVISION="$(sed -n 's/^Pkg\.Revision *= *//p' "$NDK/source.properties" 2>/dev/null | tr -d '\r' || true)"

# macOS 上没有 nproc；CI 只有 Linux，但脚本别在 Darwin 上莫名挂掉。
JOBS="$( (command -v nproc >/dev/null 2>&1 && nproc) || sysctl -n hw.ncpu 2>/dev/null || echo 2 )"

echo "==> Android tdjson: abi=$ABI api=$API_LEVEL ndk=${NDK_REVISION:-unknown} ($NDK)"
echo "==> Official build doc: https://tdlib.github.io/td/build.html"
echo "==> Extra: static OpenSSL (built by this script) + NDK sysroot zlib"

mkdir -p "$OUT_DIR/tdjson"

# ---------------------------------------------------------------------------
# 1) host 侧先把生成源码跑出来（td/generate/auto/**）
# ---------------------------------------------------------------------------
GENERATED_MARKER="$TDLIB_SRC/td/generate/auto/td/telegram/td_api.h"
if [[ -f "$GENERATED_MARKER" ]]; then
  echo "==> 生成源码已存在，跳过 host 生成：$GENERATED_MARKER"
else
  echo "==> host 构建：生成源码（prepare_cross_compiling）"
  command -v gperf >/dev/null 2>&1 || { echo "error: 缺 gperf（TDLib 生成器要用）" >&2; exit 1; }
  HOST_BUILD="${TDLIB_SRC}/build-android-host"
  rm -rf "$HOST_BUILD"
  mkdir -p "$HOST_BUILD"
  (
    cd "$HOST_BUILD"
    cmake -DCMAKE_BUILD_TYPE=Release -DTD_ENABLE_JNI=OFF "$TDLIB_SRC"
    cmake --build . --target prepare_cross_compiling -j "$JOBS"
  )
  [[ -f "$GENERATED_MARKER" ]] || {
    echo "error: host 生成阶段没产出 $GENERATED_MARKER" >&2
    find "$TDLIB_SRC/td/generate" -maxdepth 3 -name 'auto' -o -maxdepth 3 -name 'td_api*' >&2 || true
    exit 1
  }
fi

# ---------------------------------------------------------------------------
# 2) shared OpenSSL（每个 ABI 一份，缓存在 $OUT_DIR/openssl/<abi>）
# ---------------------------------------------------------------------------
# 用 `shared` 而不是 `no-shared`：产物要作为独立 .so 随包分发（见文件头 2.）。
OPENSSL_PREFIX="$OUT_DIR/openssl/$ABI"
if [[ -e "$OPENSSL_PREFIX/lib/libcrypto.so" && -e "$OPENSSL_PREFIX/lib/libssl.so" ]]; then
  echo "==> 复用已有 OpenSSL：$OPENSSL_PREFIX"
else
  echo "==> 编译 shared OpenSSL $OPENSSL_VERSION ($OPENSSL_TARGET)"
  CACHE="${OPENSSL_CACHE_DIR:-$OUT_DIR/openssl-src}"
  mkdir -p "$CACHE"
  TARBALL="$CACHE/openssl-$OPENSSL_VERSION.tar.gz"
  if [[ ! -s "$TARBALL" ]]; then
    curl -fsSL --retry 3 -o "$TARBALL.part" \
      "https://www.openssl.org/source/openssl-$OPENSSL_VERSION.tar.gz"
    mv "$TARBALL.part" "$TARBALL"
  fi
  # 官方随包发布的 sha256 侧车（拿不到就只警告：离线复用时缓存里已有源码）。
  SHA_URL="https://www.openssl.org/source/openssl-$OPENSSL_VERSION.tar.gz.sha256"
  if EXPECTED="$(curl -fsSL --max-time 30 "$SHA_URL" 2>/dev/null | awk '{print $1}' | tr -d '\r')" \
     && [[ -n "$EXPECTED" ]]; then
    ACTUAL="$(sha256sum "$TARBALL" | awk '{print $1}')"
    if [[ "$ACTUAL" != "$EXPECTED" ]]; then
      echo "error: OpenSSL 源码校验失败 $TARBALL" >&2
      echo "       expected=$EXPECTED actual=$ACTUAL" >&2
      exit 1
    fi
    echo "==> OpenSSL sha256 OK: $ACTUAL"
  else
    echo "warning: 拿不到 $SHA_URL，跳过 OpenSSL 源码校验" >&2
  fi

  SRC_DIR="$CACHE/openssl-$OPENSSL_VERSION"
  rm -rf "$SRC_DIR"
  tar -xzf "$TARBALL" -C "$CACHE"
  (
    cd "$SRC_DIR"
    PATH="$TOOLCHAIN/bin:$PATH" ./Configure "$OPENSSL_TARGET" \
      "-D__ANDROID_API__=$API_LEVEL" \
      shared no-tests no-apps no-docs \
      --prefix="$OPENSSL_PREFIX" --openssldir="$OPENSSL_PREFIX/ssl"

    # ⚠️ OpenSSL 的 make 有三个坑，都踩过：
    #
    # 1. **自更新 Makefile**：第一次 make 会重新生成 Makefile，然后要求
    #    「Please run the same make command again」。所以下面用重试循环，别当失败。
    # 2. **并行会抢 `.d.tmp`**：Makefile 里有些对象**同时挂在两个库里**
    #    （`make -n` 里同一个 `-o X.o` 出现两次），`make -j` 下两条配方会同时写/搬
    #    同一个 `X.d.tmp`，报 `mv: cannot stat 'X.d.tmp'`。16 路并行几乎必挂，
    #    这不是偶发。所以 OpenSSL 默认**串行**编（几分钟）；赶时间就传 OPENSSL_JOBS。
    # 3. `-j1` 之后仍有极小概率需要第二次 make（生成的头文件比对象新）。重试兜底。
    OPENSSL_JOBS="${OPENSSL_JOBS:-1}"
    echo "==> OpenSSL make -j$OPENSSL_JOBS（并行会抢 .d.tmp，默认串行）"
    attempt=0
    until PATH="$TOOLCHAIN/bin:$PATH" make -j "$OPENSSL_JOBS" install_sw; do
      attempt=$((attempt + 1))
      if [[ "$attempt" -ge 3 ]]; then
        echo "error: OpenSSL 构建失败（已重跑 $attempt 次）" >&2
        exit 1
      fi
      echo "==> OpenSSL make 需要重跑（第 $attempt 次）"
    done
  )
fi

OPENSSL_SO="$(find "$OPENSSL_PREFIX/lib" -maxdepth 1 -name 'libcrypto.so*' -type f | head -n1 || true)"
[[ -n "$OPENSSL_SO" ]] || { echo "error: libcrypto.so 没编出来（shared 没生效？）" >&2; ls -la "$OPENSSL_PREFIX/lib" >&2 || true; exit 1; }

# ---------------------------------------------------------------------------
# 3) 交叉编译 TDLib（tdjson）
# ---------------------------------------------------------------------------
BUILD_DIR="${TDLIB_SRC}/build-android-${ABI}"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"

CMAKE_ARGS=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_TOOLCHAIN_FILE="$NDK/build/cmake/android.toolchain.cmake"
  -DANDROID_ABI="$ABI"
  -DANDROID_PLATFORM="android-$API_LEVEL"
  # c++_shared：libc++ / libc++abi 单独一个 .so，**不**并进 tdjson。这样宿主可以把它
  # 钉住到进程结束，库里的 pthread key（API 21 的 `__cxa_thread_atexit` 兜底）不会
  # 跟着 tdjson 一起被 dlclose（见文件头 2.）。
  -DANDROID_STL=c++_shared
  # Android 15 起要求 16KB 页对齐（NDK r27+ 的开关）。
  -DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES=ON
  -DBUILD_SHARED_LIBS=ON
  -DTD_ENABLE_JNI=OFF
  -DOPENSSL_ROOT_DIR="$OPENSSL_PREFIX"
  -DOPENSSL_INCLUDE_DIR="$OPENSSL_PREFIX/include"
  -DOPENSSL_SSL_LIBRARY="$OPENSSL_PREFIX/lib/libssl.so"
  -DOPENSSL_CRYPTO_LIBRARY="$OPENSSL_PREFIX/lib/libcrypto.so"
  # 依赖全在 tdjson 自己旁边（宿主目录 / 缓存目录都是「所有 .so 同级」）。
  # 没这条，`System.load("/data/.../libtdjson.so")` 会报 libc++_shared.so / libcrypto.so 找不到。
  -DCMAKE_SHARED_LINKER_FLAGS="-Wl,-rpath,\$ORIGIN"
)

# zlib 用 NDK sysroot 里按 API 分层的 stub（Android 系统自带 libz.so）。
ZLIB_LIB="$(find "$SYSROOT/usr/lib/$TRIPLE" -maxdepth 2 -name 'libz.so' 2>/dev/null | sort | head -n1 || true)"
if [[ -n "$ZLIB_LIB" ]]; then
  echo "==> zlib: $ZLIB_LIB"
  CMAKE_ARGS+=(-DZLIB_LIBRARY="$ZLIB_LIB" -DZLIB_INCLUDE_DIR="$SYSROOT/usr/include")
else
  echo "warning: sysroot 里没找到 $TRIPLE 的 libz.so，交给 find_package(ZLIB) 自己找" >&2
fi

echo "==> cmake (android.toolchain.cmake, abi=$ABI)"
cmake "${CMAKE_ARGS[@]}" "$TDLIB_SRC" -B "$BUILD_DIR"
echo "==> cmake --build . --target tdjson"
cmake --build "$BUILD_DIR" --target tdjson -j "$JOBS"

# ---------------------------------------------------------------------------
# 4) 收集产物
# ---------------------------------------------------------------------------
MAIN=""
while IFS= read -r -d '' f; do
  if [[ ! -L "$f" ]]; then
    MAIN="$f"
    break
  fi
done < <(find "$BUILD_DIR" -type f -name 'libtdjson.so*' -print0 2>/dev/null | sort -z)
if [[ -z "$MAIN" ]]; then
  echo "error: 没产出 libtdjson.so" >&2
  find "$BUILD_DIR" -type f -name '*tdjson*' >&2 || true
  exit 1
fi
cp -aL "$MAIN" "$OUT_DIR/tdjson/libtdjson.so"
"$TOOLCHAIN/bin/llvm-strip" --strip-unneeded "$OUT_DIR/tdjson/libtdjson.so"

# 依赖单独出包：libc++_shared + OpenSSL 的 libcrypto/libssl（保留 SONAME 文件名，
# 例如 libcrypto.so.3；tdjson 的 DT_NEEDED 认的就是这个）。
# ← 这三个文件必须**和 libtdjson.so 同级**，宿主的加载器会在同一目录预加载并钉住它们。
echo "==> 收集运行时依赖"
shopt -s nullglob
for pattern in 'libcrypto.so*' 'libssl.so*'; do
  for f in "$OPENSSL_PREFIX/lib/"$pattern; do
    [[ -f "$f" ]] || continue
    base="$(basename "$f")"
    # 不带版本号的 `libcrypto.so` 通常是软链；解引用会多出一份 5 MB 的副本。
    # tdjson 的 DT_NEEDED 只认带 SONAME 的那个（如 libcrypto.so.3），所以跳过软链。
    if [[ -L "$f" ]]; then
      continue
    fi
    cp -aL "$f" "$OUT_DIR/tdjson/$base"
  done
done
shopt -u nullglob

LIBCXX="$(find "$SYSROOT/usr/lib/$TRIPLE" -maxdepth 1 -name 'libc++_shared.so' -type f 2>/dev/null | head -n1 || true)"
if [[ -z "$LIBCXX" ]]; then
  echo "error: sysroot 里没有 libc++_shared.so（$SYSROOT/usr/lib/$TRIPLE）" >&2
  exit 1
fi
cp -aL "$LIBCXX" "$OUT_DIR/tdjson/libc++_shared.so"

for f in "$OUT_DIR/tdjson/"libcrypto.so* "$OUT_DIR/tdjson/"libssl.so* "$OUT_DIR/tdjson/libc++_shared.so"; do
  [[ -f "$f" ]] && "$TOOLCHAIN/bin/llvm-strip" --strip-unneeded "$f" || true
done

LIB="$OUT_DIR/tdjson/libtdjson.so"
echo "==> llvm-readelf -h"
"$TOOLCHAIN/bin/llvm-readelf" -h "$LIB" | grep -E 'Class|Machine|Type' || true

# 现在**必须**是动态依赖：少一个就说明又并回去了（或者根本没链上）。
echo "==> DT_NEEDED（必须是动态依赖 libc++_shared / libcrypto）"
NEEDED="$("$TOOLCHAIN/bin/llvm-readelf" -d "$LIB" | grep -E 'NEEDED' || true)"
echo "$NEEDED"
for required in 'libc++_shared' 'libcrypto'; do
  if ! echo "$NEEDED" | grep -F "$required" >/dev/null; then
    echo "error: tdjson 没有动态依赖 $required（又静态并进去了？）" >&2
    exit 1
  fi
done
# DT_NEEDED 里的每个非系统库都必须真的躺在同级目录里，否则设备上 dlopen 直接失败。
while IFS= read -r dep; do
  dep="$(echo "$dep" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')"
  if [[ -z "$dep" ]]; then
    continue
  fi
  case "$dep" in
    libc.so|libdl.so|libm.so|libz.so|liblog.so|libandroid.so|ld-android.so) continue ;;
  esac
  if [[ ! -f "$OUT_DIR/tdjson/$dep" ]]; then
    echo "error: DT_NEEDED 里的 $dep 不在 $OUT_DIR/tdjson（设备上会加载失败）" >&2
    ls -la "$OUT_DIR/tdjson" >&2 || true
    exit 1
  fi
done <<< "$NEEDED"

{
  echo "platform=android"
  echo "arch=${CARCH}"
  echo "abi=${ABI}"
  echo "api_level=${API_LEVEL}"
  echo "ndk_revision=${NDK_REVISION:-unknown}"
  echo "openssl_version=${OPENSSL_VERSION}"
  echo "doc=https://tdlib.github.io/td/build.html"
  echo "official_flow=host-generate+cmake(ndk toolchain)+tdjson"
  echo "linkage=shared"
  echo "runtime_deps=libc++_shared.so,libcrypto.so,libssl.so"
  echo "zlib_note=libz.so comes from the Android system (NDK sysroot stub)"
  echo "note=libc++ (c++_shared) and OpenSSL are separate .so files next to libtdjson.so; rpath=\$ORIGIN"
} > "$OUT_DIR/tdjson/build-info.txt"

file "$LIB" || true
ls -lh "$OUT_DIR/tdjson"
echo "==> Android $ABI tdjson ready"
