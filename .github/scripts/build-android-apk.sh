#!/usr/bin/env bash
# 把 **本仓产出的 Android JNI 包** 塞进 [FuyuBot-for-Android] 的壳里，编出可安装的 APK。
#
#   JNI 包（本仓 release-bot 的 build-android 产物）
#     libfuyubotjni.so / libtdjson.so / libc++_shared.so / libcrypto.so / libssl.so
#     plugins/*.so、include/、README.txt
#         │  解压到 app/src/main/assets/fuyubot-kernel/（那是上游约定的**指定目录**，
#         │  被 .gitignore 排除、不入库，缺了它也照样能编出「没有内核的」APK）
#         ▼
#   fuyubot-<版本>-<abi>.apk
#
# 为什么内核进 assets 而不是 jniLibs：Android 10+ 禁止执行应用私有目录下的二进制，
# 但允许 `System.load()` 同目录的 .so。上游 KernelManager 因此把内核当**可替换的数据**：
# 随 APK 打包、首次启动释放到 `filesDir/fuyubot-kernel/`、版本变了就重装。
#
# ⚠️ 两处**必须按版本改写**的上游常量（不改就会安静地错）：
#   1. `data/update/UpdateChannel.kt` 的 `CURRENT_KERNEL_VERSION`：KernelManager 拿它和
#      已安装内核比版本，**相等就跳过重装**。内核升级了这行没跟上，用户装上新 APK 仍然
#      跑着 filesDir 里那份旧内核——症状是「更新了但没变化」，极难查。
#   2. `app/build.gradle.kts` 的 `versionCode` / `versionName`：不改的话每个 Release 都是
#      `1.0` / `1`，应用内的更新检查永远看不出新版本。
#   两处都用「正则命中才改、改完回读校验」的方式，命中不了就**直接报错**——
#   宁可这条 APK 编不出来，也不要发一个看起来正常、其实内核没换的包。
#
# 签名：用 apksigner 显式签（不用 AGP 的 injected signing）。keystore 类型按文件魔数
# 自动判定（PKCS12 的 DER 与 JKS 的 0xFEEDFEED 是两种容器，apksigner 拿错类型会报
# 「Keystore was tampered with, or password was incorrect」，很有迷惑性）。
#
# Args: <jni-zip> <app-repo-dir> <abi> <version> <dist-dir>
#   jni-zip        fuyubot-<版本>-android-jni-<abi>.zip（本仓 build-android 的产物）
#   app-repo-dir   FuyuBot-for-Android 的 checkout（含 gradlew、app/）
#   abi            arm64-v8a / x86_64（AGP 口径，也是 APK 文件名里的那段）
#   version        版本号（tag 名，如 v0.1.3；分支上手动跑是 dev-<短 sha>）
#   dist-dir       输出目录，最终落到 <dist>/fuyubot-<version>-<abi>.apk
#
# 环境变量（签名，缺一不可）：
#   FUYUBOT_ANDROID_KEYSTORE            keystore 文件路径
#   FUYUBOT_ANDROID_KEYSTORE_PASSWORD   store 口令
#   FUYUBOT_ANDROID_KEY_ALIAS           key alias
#   FUYUBOT_ANDROID_KEY_PASSWORD        key 口令（与 store 口令相同也照样传）
#   ANDROID_HOME / ANDROID_SDK_ROOT     二者至少一个（找 zipalign / apksigner）
set -euo pipefail

ZIP="${1:?jni zip}"
APP="${2:?fuyubot-for-android checkout}"
ABI="${3:?abi: arm64-v8a | x86_64}"
VERSION="${4:?version}"
DIST="${5:?dist dir}"

case "$ABI" in
  arm64-v8a) ARCH=aarch64 ;;
  x86_64)    ARCH=x86_64 ;;
  *) echo "error: 不支持的 abi：$ABI（只发 arm64-v8a / x86_64）" >&2; exit 1 ;;
esac

[[ -f "$ZIP" ]] || { echo "error: 找不到 JNI 包 $ZIP" >&2; exit 1; }
[[ -x "$APP/gradlew" || -f "$APP/gradlew" ]] || { echo "error: $APP 不是 FuyuBot-for-Android 的 checkout（没有 gradlew）" >&2; exit 1; }
[[ -f "$APP/app/build.gradle.kts" ]] || { echo "error: $APP 里没有 app/build.gradle.kts" >&2; exit 1; }

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
[[ -n "$SDK" && -d "$SDK" ]] || { echo "error: ANDROID_HOME / ANDROID_SDK_ROOT 没指向一个存在的 Android SDK" >&2; exit 1; }
# build-tools 取版本号最大的那一份（sort -V 对 `37.0.0` vs `36.1.0` 才排得对）。
# 目录不存在时 ls 会失败，`|| true` 兜住，交给下面的判据报错。
BUILD_TOOLS="$(ls -d "$SDK"/build-tools/*/ 2>/dev/null | sort -V | tail -1 || true)"
[[ -n "$BUILD_TOOLS" ]] || { echo "error: $SDK/build-tools 下一份 build-tools 都没有" >&2; exit 1; }
APKSIGNER="${BUILD_TOOLS%/}/apksigner"
ZIPALIGN="${BUILD_TOOLS%/}/zipalign"
[[ -x "$APKSIGNER" ]] || { echo "error: 缺 apksigner（$APKSIGNER）" >&2; exit 1; }
[[ -x "$ZIPALIGN" ]] || { echo "error: 缺 zipalign（$ZIPALIGN）" >&2; exit 1; }

# ── 1. JNI 包自检 ────────────────────────────────────────────────
# 只确认「解压出来能直接当内核用」的那几个文件在；ELF 类型/对齐/导出符号的严格校验在
# package-fuyubot-jni.sh 里已经做过一遍，这里不重复。
echo "==> 检查 JNI 包：$ZIP"
listing="$(unzip -Z1 "$ZIP")"
for required in libfuyubotjni.so libtdjson.so; do
  if ! printf '%s\n' "$listing" | grep -qx "$required"; then
    echo "error: $ZIP 里没有 $required（zip 里必须是**没有外层目录**的平铺结构）" >&2
    printf '%s\n' "$listing" | head -20 >&2
    exit 1
  fi
done
# libc++_shared 是 tdjson 的运行时依赖，缺了设备上直接 `library not found`。
# 早期做过「静态并入」的 tdjson 才没有它，这里只警告，不拦（与上游 README 的口径一致）。
if ! printf '%s\n' "$listing" | grep -qx 'libc++_shared.so'; then
  echo "warning: JNI 包里没有 libc++_shared.so（静态并入的旧 tdjson？）——设备上可能加载失败" >&2
fi
if ! printf '%s\n' "$listing" | grep -q '^plugins/.*\.so$'; then
  echo "warning: JNI 包里没有 plugins/*.so，APK 里会没有 /start、/help、/plugins" >&2
fi

# ── 2. 放进上游约定的指定目录 ─────────────────────────────────────
# assets/ 被 .gitignore 排除，checkout 后不存在，每次从头铺（避免上一次的残留混进来）。
ASSETS="$APP/app/src/main/assets/fuyubot-kernel"
rm -rf "$APP/app/src/main/assets"
mkdir -p "$ASSETS"
unzip -q "$ZIP" -d "$ASSETS"
[[ -f "$ASSETS/libfuyubotjni.so" && -f "$ASSETS/libtdjson.so" ]] || {
  echo "error: 解压后 $ASSETS 里缺内核主库——JNI 包的目录结构变了？" >&2
  find "$ASSETS" -maxdepth 2 | head -20 >&2
  exit 1
}
echo "==> 内核已铺进 app/src/main/assets/fuyubot-kernel/"
ls -lh "$ASSETS"

# ── 3. 按版本改写上游的两处常量 ───────────────────────────────────
# 用 python 做「按行正则替换 + 回读校验」：sed 在不同平台的转义差异太多，
# 而这两处一旦改错就是「APK 能装、内核没换」这种最贵的 bug。
#
# 先找一个**真能用**的 python：Windows 上 `python3` 常常只是应用商店的转发壳，
# 它 exit 0 却什么都不执行——所以必须让它真的打印一次版本号来确认。
# （同 `.github/scripts/package-fuyubot-jni.sh` 的 PY 探测，那边踩过同一个坑。）
PY="${PYTHON:-}"
if [[ -z "$PY" ]]; then
  for candidate in python3 python; do
    if [[ "$("$candidate" -c 'import sys; print(sys.version_info[0])' 2>/dev/null || true)" == "3" ]]; then
      PY="$candidate"
      break
    fi
  done
fi
[[ -n "$PY" ]] || { echo "error: 找不到可用的 python3（改写版本常量需要它）" >&2; exit 1; }

echo "==> 改写版本常量（version=$VERSION）"
"$PY" - "$APP" "$VERSION" <<'PY'
import re
import sys
from pathlib import Path

app = Path(sys.argv[1])
version = sys.argv[2]

# versionName 给用户看；versionCode 是 Android 比大小的整数。
# 只有 `v1.2.3` 这种**以数字开头**的才算语义版本，拆成 major*10000+minor*100+patch；
# `dev-abc1234` 这类手动构建退回 1——反正 publish 只认 tag，这种包不会进 Release，
# 而把 sha 里的数字当版本号会让 versionCode 变成一个毫无意义的巨大整数。
name = version[1:] if version[:1] in ("v", "V") else version
if re.match(r"\d+(\.\d+)*", name):
    parts = [int(part) for part in name.split(".")[:3]]
    parts += [0] * (3 - len(parts))
    code = max(parts[0] * 10000 + parts[1] * 100 + parts[2], 1)
else:
    code = 1


def patch(relative, pattern, replacement, expected, what):
    """替换**恰好一处**并回读校验。

    `expected` 是替换后必须能在文件里看到的子串——这一步不能省：
    「正则没命中就不改、静默通过」正是我们要避免的。命中数不是 1 就直接失败。
    """
    path = app / relative
    text = path.read_text(encoding="utf-8")
    updated, count = re.subn(pattern, replacement, text, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(
            f"error: 在 {relative} 里没找到 {what}（命中 {count} 次）。\n"
            "       上游改了写法？把本脚本的 pattern 一起改掉再来。\n"
            "       别跳过——跳过会发出一个「内核版本没变 / 版本号没动」的 APK。"
        )
    if expected not in updated:
        raise SystemExit(f"error: {relative} 的 {what} 改写后没生效")
    path.write_text(updated, encoding="utf-8")


# 1) 内核版本：KernelManager 拿它和 filesDir 里已装的内核比版本，**相等就跳过重装**。
#    内核升级了这行没跟上 = 用户装上新 APK 仍在跑旧内核（症状是「更新了但没变化」）。
patch(
    "app/src/main/java/com/CatMoeCircle/fuyutd/data/update/UpdateChannel.kt",
    r'^const val CURRENT_KERNEL_VERSION = ".*"$',
    f'const val CURRENT_KERNEL_VERSION = "{name}"',
    f'CURRENT_KERNEL_VERSION = "{name}"',
    "const val CURRENT_KERNEL_VERSION",
)

# 2) APK 自己的版本号：应用内的更新检查比的就是它。
patch(
    "app/build.gradle.kts",
    r"^([ \t]*)versionCode = \d+$",
    lambda m: f"{m.group(1)}versionCode = {code}",
    f"versionCode = {code}",
    "versionCode = <整数>",
)
patch(
    "app/build.gradle.kts",
    r'^([ \t]*)versionName = ".*"$',
    lambda m: f'{m.group(1)}versionName = "{name}"',
    f'versionName = "{name}"',
    'versionName = "<字符串>"',
)

print(f"OK versionName={name} versionCode={code} kernel={name}")
PY

# ── 4. 编译 ──────────────────────────────────────────────────────
# `--no-daemon`：CI 里每次都是一次性进程，daemon 只会多占内存、还要负责收尸。
# 配置缓存是上游 gradle.properties 开的，保留（同一个 checkout 只编一次，不影响）。
echo "==> gradlew :app:assembleRelease（abi=$ABI）"
chmod +x "$APP/gradlew"
(
  cd "$APP"
  ./gradlew --no-daemon --console=plain :app:assembleRelease
)

# ── 5. 找 APK、对齐、签名 ────────────────────────────────────────
# 上游没有 signingConfigs，所以产物是**未签名**的那一份；文件名在 AGP 各版本间变过
# （`app-release-unsigned.apk` / `app-release.apk`），别写死。
# 优先挑 `-unsigned`：万一同目录里还留着别的产物（增量构建），也不会误签一个已签名的。
APK_DIR="$APP/app/build/outputs/apk/release"
apk="$(find "$APK_DIR" -maxdepth 1 -type f -name '*-unsigned.apk' | head -n1 || true)"
[[ -n "$apk" ]] || apk="$(find "$APK_DIR" -maxdepth 1 -type f -name '*.apk' | head -n1 || true)"
[[ -n "$apk" ]] || { echo "error: $APK_DIR 下没有 APK" >&2; ls -la "$APK_DIR" >&2 2>/dev/null || true; exit 1; }
echo "==> 待签名产物：$apk"
ls -lh "$APK_DIR"

KEYSTORE="${FUYUBOT_ANDROID_KEYSTORE:-}"
for required in FUYUBOT_ANDROID_KEYSTORE FUYUBOT_ANDROID_KEYSTORE_PASSWORD \
                FUYUBOT_ANDROID_KEY_ALIAS FUYUBOT_ANDROID_KEY_PASSWORD; do
  if [[ -z "${!required:-}" ]]; then
    echo "error: 缺环境变量 $required（签名用）。" >&2
    echo "       仓库 Secrets 里配 ANDROID_KEYSTORE_BASE64 / ANDROID_KEYSTORE_PASSWORD /" >&2
    echo "       ANDROID_KEY_ALIAS / ANDROID_KEY_PASSWORD，见 release-bot.yml 的 build-apk job。" >&2
    exit 1
  fi
done
[[ -f "$KEYSTORE" ]] || { echo "error: keystore 文件不存在：$KEYSTORE" >&2; exit 1; }

# keystore 类型按**文件魔数**判，不看扩展名：PKCS12 是 DER（0x30 0x82 开头），
# JKS 是 0xFEEDFEED。扩展名经常骗人（.jks 里装的是 PKCS12 很常见），
# 而 apksigner 拿错类型只会报「密码不对」，排查成本很高。
magic="$(od -An -tx1 -N2 "$KEYSTORE" | tr -d ' \n')"
case "$magic" in
  feed*) ks_type=JKS ;;
  # DER 的 SEQUENCE 头是 0x30；长度前缀从 0x80 起（短格式更小），所以只认 `30` 开头。
  30*) ks_type=PKCS12 ;;
  *)
    echo "error: 认不出 keystore 容器（头两个字节 0x$magic）。JKS 是 feedfeed，PKCS12 是 30xx" >&2
    exit 1
    ;;
esac
echo "==> keystore 类型：$ks_type（魔数 0x$magic）"

mkdir -p "$DIST"
DIST="$(cd "$DIST" && pwd)"
ALIGNED="$(mktemp -d)/aligned.apk"
# 文件名口径：`fuyubot-<版本>-<abi>.apk`（例 `fuyubot-v0.1.3-arm64-v8a.apk`）。
# 不加 `-android-` 也不会和别的资产撞：可执行文件包是 `fuyubot-<版本>-android-<平台>.zip`，
# JNI 包是 `fuyubot-<版本>-android-jni-<abi>.zip`，基名都不同。
SIGNED="$DIST/fuyubot-${VERSION}-${ABI}.apk"

# 先对齐再签名：v2 / v3 签名覆盖的是整个文件，签完再对齐会直接把签名弄废。
# `-P 16` 把未压缩的 .so 对齐到 16KiB 页边界——我们自己的 .so 就是按 16KB 页编的
# （`-Wl,-z,max-page-size=16384`），Android 15 的 16KB 页设备上 mmap 才成立。
# 不用 `-p`：官方文档已标注它被 `-P 16` 取代。
"$ZIPALIGN" -P 16 -f 4 "$apk" "$ALIGNED"

# 口令走 `env:` 而不写进命令行：`pass:` 的形式会让口令出现在 `ps` 与 CI 日志里。
# 只开 v2 + v3：壳的 minSdk 是 33（Android 13），v1（JDK 的 jar 签名方案）根本不会被用到，
# 开着只会往包里多塞一份 META-INF 的摘要。
"$APKSIGNER" sign \
  --ks "$KEYSTORE" \
  --ks-type "$ks_type" \
  --ks-key-alias "$FUYUBOT_ANDROID_KEY_ALIAS" \
  --ks-pass env:FUYUBOT_ANDROID_KEYSTORE_PASSWORD \
  --key-pass env:FUYUBOT_ANDROID_KEY_PASSWORD \
  --v1-signing-enabled false \
  --v2-signing-enabled true \
  --v3-signing-enabled true \
  --in "$ALIGNED" \
  --out "$SIGNED"

# ── 6. 校验 ──────────────────────────────────────────────────────
# 只 ls 不算验过：apksigner verify 会真的解出签名块、核对证书链与摘要。
echo "==> 校验签名"
VERIFY_LOG="$(mktemp)"
if ! "$APKSIGNER" verify --print-certs --verbose "$SIGNED" > "$VERIFY_LOG" 2>&1; then
  echo "error: apksigner verify 失败：" >&2
  cat "$VERIFY_LOG" >&2
  exit 1
fi
grep -E "Verified using v[123] scheme|Signer #1 certificate DN" "$VERIFY_LOG" || true
rm -f "$VERIFY_LOG"

# 内核文件确实进了 APK（这是「构建完成」的唯一判据，与上游 README 的自检一致）。
echo "==> 校验内核已在 APK 里"
unzip -l "$SIGNED" | grep -E 'assets/fuyubot-kernel/(libfuyubotjni|libtdjson)\.so' || {
  echo "error: APK 里没有 assets/fuyubot-kernel/ 的内核文件——内核没铺进去？" >&2
  exit 1
}

rm -rf "$(dirname "$ALIGNED")"

echo "==> ${SIGNED##*/} ready（$(du -h "$SIGNED" | cut -f1)）"
