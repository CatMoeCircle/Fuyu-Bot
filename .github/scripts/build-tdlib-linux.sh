#!/usr/bin/env bash
# TDLib on Linux — follow https://tdlib.github.io/td/build.html
# Official flow: install deps → cmake Release → prepare_cross_compiling → tdjson
#
# ⚠️ 2026 起**不再**把 OpenSSL/zlib 静态并进 tdjson：改成动态依赖，并把
#    libssl.so* / libcrypto.so* / libz.so* **随包发**（与 tdjson 同级）。
#    两条理由：
#      1. 静态并入的那份 OpenSSL 拿不到系统的安全更新，CVE 只能等我们重发资产；
#      2. 宿主进程里若有别的 OpenSSL（插件、Python 的 `_ssl`…），静态并入的那份
#         会把符号抢过去，出问题的地方离根因十万八千里。
#    加载侧：`fuyubot-tdlib::loader::preload_runtime_deps` 在 `dlopen(tdjson)` 之前
#    按 soname 把同目录的这几份钉住（绝对路径 dlopen 不会搜库自己所在的目录）。
# Args: <tdlib-src-dir> <output-dir> <arch>
set -euo pipefail

TDLIB_SRC="${1:?tdlib source dir}"
OUT_DIR="${2:?output dir}"
ARCH="${3:?arch: x86_64 | arm64}"

case "$ARCH" in
  x86_64) CMAKE_PROCESSOR=x86_64 ;;
  arm64)  CMAKE_PROCESSOR=aarch64 ;;
  *)
    echo "unsupported arch: $ARCH" >&2
    exit 1
    ;;
esac

echo "==> Linux TDLib arch=${ARCH}"
echo "==> Official build doc: https://tdlib.github.io/td/build.html"
echo "==> OpenSSL/zlib stay dynamic (shipped next to libtdjson.so)"

# Official deps (Ubuntu/Debian): gperf cmake clang/c++ zlib1g-dev libssl-dev
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  gperf cmake ninja-build \
  clang g++ \
  zlib1g-dev libssl-dev \
  perl make curl ca-certificates

# Official: mkdir build && cd build && cmake -DCMAKE_BUILD_TYPE=Release ..
BUILD_DIR="${TDLIB_SRC}/build-linux-${ARCH}"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

CMAKE_ARGS=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_SYSTEM_PROCESSOR="$CMAKE_PROCESSOR"
  -DBUILD_SHARED_LIBS=ON
  -DTD_ENABLE_JNI=OFF
)

# Official uses clang when available
if command -v clang >/dev/null 2>&1 && command -v clang++ >/dev/null 2>&1; then
  CMAKE_ARGS+=(-DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++)
fi

echo "==> cmake -DCMAKE_BUILD_TYPE=Release .."
cmake "${CMAKE_ARGS[@]}" "$TDLIB_SRC"

# Official build sequence
echo "==> cmake --build . --target prepare_cross_compiling"
cmake --build . --target prepare_cross_compiling -j "$(nproc)" || true
echo "==> cmake --build . --target tdjson"
cmake --build . --target tdjson -j "$(nproc)"

echo "==> collect artifacts"
mkdir -p "$OUT_DIR/tdjson"
MAIN=""
while IFS= read -r -d '' f; do
  if [[ ! -L "$f" ]]; then
    MAIN="$f"
    break
  fi
done < <(find "$BUILD_DIR" -type f -name 'libtdjson.so*' -print0 2>/dev/null | sort -z)
if [[ -z "$MAIN" ]]; then
  MAIN="$(find "$BUILD_DIR" -type f -name 'libtdjson.so' | head -n1 || true)"
fi
if [[ -z "$MAIN" ]]; then
  echo "error: no libtdjson produced" >&2
  find "$BUILD_DIR" -type f -name '*tdjson*' >&2 || true
  exit 1
fi
cp -aL "$MAIN" "$OUT_DIR/tdjson/libtdjson.so"
strip --strip-unneeded "$OUT_DIR/tdjson/libtdjson.so" 2>/dev/null || true

# 依赖**必须是动态的**：静态并入会让「随包发的那几份 .so」形同虚设，
# 也拿不到系统的安全更新。ldd 里看不到它们说明 OpenSSL/zlib 还被静态链着。
echo "==> ldd (must list libssl/libcrypto/libz)"
ldd_out="$(ldd "$OUT_DIR/tdjson/libtdjson.so" 2>&1 || true)"
echo "$ldd_out"
for dep in libssl.so libcrypto.so libz.so; do
  if ! grep -q "$dep" <<< "$ldd_out"; then
    echo "error: $dep 不是动态依赖——OpenSSL/zlib 又被静态并进去了？" >&2
    exit 1
  fi
done

# 按 ldd 解析出来的**真实路径**收集（多架构目录、带版本号，别按名字猜），
# 用它自己的文件名（= SONAME）落盘：加载器靠 soname 复用已加载的映射。
#
# ⚠️ 必须 `cp -L` 解引用：Debian/Ubuntu 上 `libz.so.1` 是**符号链接**
# （→ `libz.so.1.2.11`），`cp -a` 只拷链接本身，包里就变成一个悬空链接——
# 下游 `-f` 判断直接跳过、运行时 dlopen 找不到，而且**不报错**（2026 踩过）。
RUNTIME_DEPS=()
while read -r name arrow resolved rest; do
  case "$name" in
    libssl.so*|libcrypto.so*|libz.so*)
      [[ "$arrow" == "=>" && -e "$resolved" ]] || continue
      dep_name="$(basename "$resolved")"
      cp -aL "$resolved" "$OUT_DIR/tdjson/$dep_name"
      if [[ ! -f "$OUT_DIR/tdjson/$dep_name" ]]; then
        echo "error: $dep_name 拷贝后不是普通文件（解引用失败）" >&2
        exit 1
      fi
      RUNTIME_DEPS+=("$dep_name")
      ;;
  esac
done <<< "$ldd_out"
for need in libssl.so libcrypto.so libz.so; do
  found=0
  for dep in "${RUNTIME_DEPS[@]}"; do
    if [[ "$dep" == "$need"* ]]; then
      found=1
    fi
  done
  if [[ "$found" -eq 0 ]]; then
    echo "error: 没收集到 $need*（拿到：${RUNTIME_DEPS[*]:-无}）" >&2
    exit 1
  fi
done
echo "==> runtime deps: ${RUNTIME_DEPS[*]}"

{
  echo "platform=linux"
  echo "arch=${ARCH}"
  echo "doc=https://tdlib.github.io/td/build.html"
  echo "official_flow=cmake+prepare_cross_compiling+tdjson"
  echo "linkage=shared"
  echo "runtime_deps=${RUNTIME_DEPS[*]}"
  echo "note=OpenSSL/zlib stay dynamic; libssl/libcrypto/libz shipped next to libtdjson.so"
} > "$OUT_DIR/tdjson/build-info.txt"

file "$OUT_DIR/tdjson/libtdjson.so" || true
ls -lh "$OUT_DIR/tdjson"
echo "==> Linux ${ARCH} tdjson ready"
