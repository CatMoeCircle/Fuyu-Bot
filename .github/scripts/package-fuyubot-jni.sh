#!/usr/bin/env bash
# 把 `fuyubotjni` 的交叉编译产物打成 **Android JNI 包**（只有 .so，没有 apk、没有 UI）：
#
#   fuyubot-<版本>-android-jni-<abi>.zip
#     libfuyubotjni.so   ← 宿主 System.loadLibrary("fuyubotjni")
#     libtdjson.so       ← 同 ABI，必须一起给
#     plugins/*.so       ← 可选的内置插件（/start、/help、/plugins）
#     include/fuyubot_jni.h
#     README.txt
#
# zip 里**没有外层目录**（与桌面 / Android 可执行文件包的口径一致）：解压到哪，文件就在哪。
#
# Args: <target-triple> <abi> <tdlib-version> <tdlib-commit> <dist-dir> [tdjson-source-dir] [package-name]
#   target-triple      aarch64-linux-android / x86_64-linux-android
#   abi                AGP 口径：arm64-v8a / x86_64（也是 jniLibs 的目录名）
#   tdjson-source-dir  含 libtdjson.so 的目录（默认 .tdlib-cache/<version>/android-<arch>）
#   package-name       包名（默认 `fuyubot-android-jni-<abi>`；release-bot 传带版本号的名字）
set -euo pipefail

TRIPLE="${1:?target triple}"
ABI="${2:?abi: arm64-v8a | x86_64}"
VERSION="${3:?tdlib version}"
COMMIT="${4:?tdlib commit}"
DIST="${5:?dist dir}"
TDLIB_DIR="${6:-}"
PKG="${7:-fuyubot-android-jni-${ABI}}"

# `DIST` 转成绝对路径：下面要在**包目录里** cd 一次再打 zip，相对路径会变成
# `dist/<包>/dist/<包>.zip`（真 zip 直接报「目录不存在」）。
mkdir -p "$DIST"
DIST="$(cd "$DIST" && pwd)"

# `arch`（cargo 口径）与 `abi`（AGP 口径）是两张表，别混着用。
case "$ABI" in
  arm64-v8a) ARCH=aarch64 ;;
  x86_64)    ARCH=x86_64 ;;
  *) echo "error: 不支持的 abi：$ABI（只发 arm64-v8a / x86_64）" >&2; exit 1 ;;
esac

[[ -n "$TDLIB_DIR" ]] || TDLIB_DIR=".tdlib-cache/${VERSION}/android-${ARCH}"

LIB="target/${TRIPLE}/release/libfuyubotjni.so"
STAGE="${DIST}/${PKG}"
PLUGIN_DIR="target/${TRIPLE}/release"
HEADER="crates/fuyubot-android/include/fuyubot_jni.h"

# JNI 绑定类的内部名：与构建时的 `FUYUBOT_JNI_CLASS` 用同一个变量（默认值是库里的默认值）。
# 用自定义包名编译过，就必须在打包时也给它，否则下面的绑定表校验会报「找不到类名」。
JNI_CLASS="${FUYUBOT_JNI_CLASS:-com/catmoecircle/fuyubot/runtime/Native}"
JNI_CLASS_DOTTED="${JNI_CLASS//\//.}"

[[ -f "$LIB" ]] || {
  echo "error: 找不到 $LIB（先 cargo build --release --target $TRIPLE -p fuyubot-android）" >&2
  exit 1
}
[[ -d "$TDLIB_DIR" && -f "$TDLIB_DIR/libtdjson.so" ]] || {
  echo "error: $TDLIB_DIR 里没有 libtdjson.so（第 6 个参数是**目录**，不是包名）" >&2
  ls -la "$TDLIB_DIR" >&2 2>/dev/null || true
  exit 1
}
[[ -f "$HEADER" ]] || { echo "error: 找不到头文件 $HEADER" >&2; exit 1; }

rm -rf "$STAGE"
mkdir -p "$STAGE/plugins" "$STAGE/include"

cp "$LIB" "$STAGE/libfuyubotjni.so"
cp "$TDLIB_DIR/libtdjson.so" "$STAGE/libtdjson.so"
cp "$HEADER" "$STAGE/include/fuyubot_jni.h"

# tdjson 的运行时依赖（独立 .so）：必须和 libtdjson.so 同级。
# 本库（libfuyubotjni.so）自己**不**依赖它们；下面那段 ELF 校验会确认这一点。
shopt -s nullglob
for pattern in 'libc++_shared.so*' 'libcrypto.so*' 'libssl.so*'; do
  for f in "$TDLIB_DIR/"$pattern; do
    [[ -f "$f" ]] || continue
    cp -aL "$f" "$STAGE/$(basename "$f")"
  done
done
shopt -u nullglob
if [[ ! -f "$STAGE/libc++_shared.so" ]]; then
  echo "warning: $TDLIB_DIR 里没有 libc++_shared.so——这是「静态并入」的旧 tdjson 产物；" >&2
  echo "         新包应带 libc++_shared / libcrypto / libssl 三个独立 .so（见 build-tdlib-android.sh）" >&2
fi

# 内置插件：Linux 产物带 `lib` 前缀，包里一律**无前缀**（与可执行文件包的口径一致）。
for stem in fuyubot_plugin_start fuyubot_plugin_help fuyubot_plugin_manage; do
  src="$PLUGIN_DIR/${stem}.so"
  [[ -f "$src" ]] || src="$PLUGIN_DIR/lib${stem}.so"
  if [[ -f "$src" ]]; then
    cp "$src" "$STAGE/plugins/${stem}.so"
  else
    echo "warning: 缺内置插件 ${stem}.so（带 lib 前缀的也试过了），本次不带它" >&2
  fi
done

# 用 printf 而不是 heredoc：YAML 块标量会把内容整体缩进，缩进的 EOF 不会终止 heredoc。
printf '%s\n' \
  "fuyubot for Android —— JNI 运行时（libfuyubotjni.so），**不是 apk、也不是可执行文件**" \
  "" \
  "外置 TDLib：${VERSION}（commit ${COMMIT}）；libtdjson.so 必须与 libfuyubotjni.so 同 ABI。" \
  "" \
  "装法：" \
  "  1) 全部 .so → app/src/main/jniLibs/${ABI}/：" \
  "     libfuyubotjni.so、libtdjson.so，以及 tdjson 的三个依赖 libc++_shared.so / libcrypto.so* / libssl.so*" \
  "     （少了依赖，tdjson 在设备上会直接报 library not found）" \
  "  2) plugins/*.so（可选）→ 拷到你的 <root>/plugins/ 再 chmod 0444（targetSdk 34+ 的只读要求）" \
  "  3) 代码里**先加载依赖、再加载 tdjson、最后加载 fuyubotjni**：" \
  "     System.load(File(nativeLibraryDir, \"libc++_shared.so\").absolutePath)" \
  "     System.load(File(nativeLibraryDir, \"libcrypto.so.3\").absolutePath)   // 版本号按产物实际文件名" \
  "     System.load(File(nativeLibraryDir, \"libssl.so.3\").absolutePath)" \
  "     System.loadLibrary(\"tdjson\"); System.loadLibrary(\"fuyubotjni\")" \
  "     然后调 ${JNI_CLASS_DOTTED}（契约见 include/fuyubot_jni.h 文件头）" \
  "     （包名不是符号名的一部分：构建时给 FUYUBOT_JNI_CLASS=com/你的/包/Native 就能换）" \
  "" \
  "必做（库管不着，得你在 App 里做）：" \
  "  - <root> 用 filesDir（/sdcard 是 noexec，TDLib 也写不进去）；" \
  "  - 前台 Service + WakeLock 兜住进程，onDestroy 里调 nativeStop()；" \
  "  - options 里给 api_id / api_hash；bot 账号再给 bot_token（不给就走手机号/扫码登录）；" \
  "  - lib_dir 可不填（先 loadLibrary(\"tdjson\") 就行），填 nativeLibraryDir 更稳；" \
  "  - 开了 R8 / ProGuard 就 keep 绑定类，混淆会改类名/方法名，绑定是运行期按名字做的：" \
  "      -keep class ${JNI_CLASS_DOTTED} { *; }" \
  "" \
  "保活是 App 的事：本库不声明任何 Android 组件 / 权限 / 唤醒锁，" \
  "只提供幂等 start、必须被调的 stop、以及 onState 状态回调。" \
  "日志在 <root>/logs/fuyubot.log（应用进程的 stdout/stderr 是 /dev/null，只能落文件）。" \
  "" \
  "需要 API ${ANDROID_API_LEVEL:-21}+；文档：https://catmoecircle.github.io/FuyuBot-docs/" \
  > "$STAGE/README.txt"

# ── 校验（别只 ls）────────────────────────────────────────────────
# 找一个**真能用**的 python：Windows 上 `python3` 可能只是应用商店的转发壳，
# 它 exit 0 却什么都不执行——所以必须让它真的打印一次版本号来确认。
PY="${PYTHON:-}"
if [[ -z "$PY" ]]; then
  for candidate in python3 python; do
    if [[ "$("$candidate" -c 'import sys; print(sys.version_info[0])' 2>/dev/null || true)" == "3" ]]; then
      PY="$candidate"
      break
    fi
  done
fi
[[ -n "$PY" ]] || { echo "error: 找不到可用的 python3（packaging 的校验步骤需要它）" >&2; exit 1; }

echo "==> 校验 libfuyubotjni.so（ELF 类型 / 机器码 / 16KB 页 / 导出符号 / JNI 绑定表）"
"$PY" - "$STAGE" "$ARCH" "$JNI_CLASS" <<'PY'
import struct
import sys
from pathlib import Path

stage = Path(sys.argv[1])
arch = sys.argv[2]
jni_class = sys.argv[3]

lib = stage / "libfuyubotjni.so"
data = lib.read_bytes()
if data[:4] != b"\x7fELF":
    raise SystemExit(f"{lib} 不是 ELF（Android 目标没生效？）")
if data[4] != 2 or data[5] != 1:
    raise SystemExit(f"期望 ELF64 小端，实际 class={data[4]} data={data[5]}")

e_type = struct.unpack_from("<H", data, 16)[0]
e_machine = struct.unpack_from("<H", data, 18)[0]
expected = {"aarch64": 183, "x86_64": 62}[arch]
if e_machine != expected:
    raise SystemExit(f"e_machine 不符：{e_machine}（期望 {expected}，arch={arch}）")
# 3 = ET_DYN：共享库。**不许**是可执行文件（2）——那是可执行文件包的形态。
if e_type != 3:
    raise SystemExit(f"e_type={e_type}，JNI 包里的这个必须是共享库（3）")

phoff = struct.unpack_from("<Q", data, 32)[0]
phentsize = struct.unpack_from("<H", data, 54)[0]
phnum = struct.unpack_from("<H", data, 56)[0]
loads = 0
for i in range(phnum):
    off = phoff + i * phentsize
    p_type = struct.unpack_from("<I", data, off)[0]
    p_align = struct.unpack_from("<Q", data, off + 48)[0]
    if p_type == 1:  # PT_LOAD
        loads += 1
        if p_align < 0x4000:
            raise SystemExit(
                f"PT_LOAD 对齐只有 0x{p_align:x}：Android 15 的 16KB 页要求 >= 0x4000"
                "（构建时加 -C link-arg=-Wl,-z,max-page-size=16384）"
            )
if loads == 0:
    raise SystemExit("没有 PT_LOAD 段，这不像一个共享库")

symbols = [
    "fuyubot_abi_version", "fuyubot_start", "fuyubot_stop",
    "fuyubot_is_running", "fuyubot_answer", "fuyubot_last_error",
    "fuyubot_plugins", "fuyubot_plugin_info",
    "fuyubot_auth_state", "fuyubot_connection_state",
    "fuyubot_td_request",
    "JNI_OnLoad",
]
missing = [name for name in symbols if (name.encode() + b"\0") not in data]
if missing:
    raise SystemExit("导出符号缺失：" + "、".join(missing))

# JNI 方法走 `RegisterNatives`，**不再有 `Java_...` 导出符号**。能查的只有类名：
# 方法名（"nativeVersion" 之类，13~17 字节）在 x86_64 上会被 LLVM 拆成
# `mov` 指令的立即数，**不一定以连续字节出现**（aarch64 上才是连续的），按字节找会误报。
# 方法名 + 签名由 `cargo test -p fuyubot-android` 的 `jni_method_table_matches_the_documented_contract` 钉住；
# 这里只确认「包名没配错」和「名字绑定已清干净」。
if b"Java_com_fuyubot" in data or b"Java_com_catmoecircle" in data:
    raise SystemExit("还在导出 Java_... 名字绑定（应该已经是 RegisterNatives）")
binding = jni_class.encode()
if binding not in data:
    raise SystemExit(
        "找不到 JNI 绑定类名（FindClass 的入参）：" + jni_class
        + "（构建时的 FUYUBOT_JNI_CLASS 必须与打包时一致）"
    )

# ⚠️ **不要**在这里按字节搜 `libc++_shared.so` / `libcrypto.so` / `libssl.so` 来断言
# 「静态合并失效」。这三个名字是 loader 里**正常的字符串字面量**：
#   crates/fuyubot-tdlib/src/loader.rs 的 siblings_to_preload 拿它们挑要预加载的同级 .so
#   （`name == "libc++_shared.so"` / `starts_with("libcrypto.so")` …）。
# 所以它们**必然**出现在 .so 的字符串表里，按字节搜永远是假阳性。
# 2026 踩过：aarch64 上这些 13~16 字节的字面量被 LLVM 拆成 `mov` 立即数、恰好不连续，
# 检查「通过」；x86_64 上连续存放，同一个产物直接报「依赖了 Android 上没有的库」——
# 差异只在架构，不在产物对不对，白白让 x86_64 的 JNI 包 + APK 挂了一轮。
#
# 真正的判据是下面的 `llvm-readelf -d`：读 **DT_NEEDED**（真实动态依赖），
# 白名单只放 bionic 的 libc / libm / libdl / liblog。没有 llvm-readelf 时**跳过**，
# 不做字节兜底——宁可少一条检查，也不要一条恒假的检查。

print(f"OK ELF64 {arch} e_type={e_type} e_machine={e_machine} PT_LOAD={loads}"
      f" 符号={len(symbols)} 类名={jni_class}")
PY

# 有 llvm-readelf 就精确读 DT_NEEDED（真实动态依赖），没有就跳过。
# ⚠️ 跳过时**不做**按字节搜库名的兜底：那三个名字是 loader 的正常字符串字面量，见上面的注释。
# NDK 的 prebuilt 目录名跟宿主走：CI 是 linux-x86_64，本机 Windows 是 windows-x86_64。
NDK_HOST="linux-x86_64"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) NDK_HOST="windows-x86_64" ;;
esac
READELF="${ANDROID_NDK_HOME:-/nonexistent}/toolchains/llvm/prebuilt/${NDK_HOST}/bin/llvm-readelf"
[[ -x "$READELF" ]] || READELF="${READELF}.exe"
if [[ -x "$READELF" ]]; then
  NEEDED="$("$READELF" -d "$STAGE/libfuyubotjni.so" | grep -E 'NEEDED' || true)"
  echo "$NEEDED"
  bad="$(echo "$NEEDED" | grep -Ev 'libc\.so|libm\.so|libdl\.so|liblog\.so' || true)"
  if [[ -n "$bad" ]]; then
    echo "error: DT_NEEDED 里有白名单外的库（只允许 bionic 的 libc/libm/libdl/liblog）：" >&2
    echo "$bad" >&2
    exit 1
  fi
else
  echo "warning: 没找到 llvm-readelf（ANDROID_NDK_HOME 没设？），跳过 DT_NEEDED 检查" >&2
fi

# ── 打包：zip 里没有外层目录 ───────────────────────────────────────
( cd "$STAGE" && zip -q -r -X "${DIST}/${PKG}.zip" . )
rm -rf "$STAGE"

echo "==> ${PKG}.zip ready"
unzip -l "${DIST}/${PKG}.zip" || true
