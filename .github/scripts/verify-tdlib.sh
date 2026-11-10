#!/usr/bin/env bash
# Verify a packaged tdjson archive: magic bytes, arch match, static-link notes.
#
# ⚠️ **不按扩展名判断格式**：发布资产的名字就是 loader 的 `asset_name()`
#    （例如 `TDLib-windows-x86_64`，**不带扩展名**），所以只能看内容。
#    zip 会被**直接拒绝**：`extract_library` 只认裸库和 `.tar.gz`，
#    收到 zip 会当裸库原样写出（坏文件且不报错）。
#
# Args: <archive> <platform> <arch> [asset-name]
set -euo pipefail

ARCHIVE="${1:?archive path}"
PLATFORM="${2:?platform}"
ARCH="${3:?arch}"
ASSET_NAME="${4:-}"

if [[ ! -f "$ARCHIVE" ]]; then
  echo "error: archive not found: $ARCHIVE" >&2
  exit 1
fi

if [[ -n "$ASSET_NAME" && "$(basename "$ARCHIVE")" != "$ASSET_NAME" ]]; then
  echo "error: archive basename '$(basename "$ARCHIVE")' != expected asset name '$ASSET_NAME'" >&2
  exit 1
fi

# ⚠️ 同 `package-tdlib.sh`：Windows runner 传进来的是 `D:/...`，而 GNU tar 把
#    `host:path` 当**远程归档**（`tar (child): Cannot connect to D: resolve failed`）。
#    统一转成 POSIX 形态再用。
if command -v cygpath >/dev/null 2>&1; then
  ARCHIVE="$(cygpath -u "$ARCHIVE")"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# 看内容，不看扩展名。
MAGIC="$(head -c2 "$ARCHIVE" | od -An -tx1 | tr -d ' \n')"
case "$MAGIC" in
  1f8b)
    tar -xzf "$ARCHIVE" -C "$WORK"
    ;;
  504b)
    echo "error: $ARCHIVE 是 zip——loader 的 extract_library 只认「裸库」和 .tar.gz" >&2
    echo "       zip 会被当成裸库原样写出，得到一个坏文件而且**不报错**；请改用 tar.gz" >&2
    exit 1
    ;;
  *)
    echo "error: $ARCHIVE 既不是 gzip 也不是 zip（magic=$MAGIC）" >&2
    exit 1
    ;;
esac

LIB=""
for name in libtdjson.so libtdjson.dylib tdjson.dll; do
  found="$(find "$WORK" -type f -name "$name" | head -n1 || true)"
  if [[ -n "$found" ]]; then
    LIB="$found"
    break
  fi
done
if [[ -z "$LIB" ]]; then
  echo "error: tdjson library missing in $ARCHIVE" >&2
  find "$WORK" -type f >&2 || true
  exit 1
fi

if [[ ! -f "$(dirname "$LIB")/VERSION.json" ]]; then
  echo "error: VERSION.json missing next to $LIB" >&2
  exit 1
fi

# 每个平台期望的链接形态是**契约**：
#   - Windows / macOS：OpenSSL/zlib 静态并入 → 包里必须**没有**独立依赖；
#   - Linux / Android：动态依赖，libssl / libcrypto（+ Linux 的 libz）必须随包、
#     与 tdjson 同级（loader 会在 dlopen 之前预加载它们）。
DIR="$(dirname "$LIB")"
if [[ "$PLATFORM" != "android" && "$PLATFORM" != "linux" ]]; then
  for bad in libssl.so libssl.so.3 libcrypto.so libcrypto.so.3 libz.so \
    libssl.3.dylib libcrypto.3.dylib libssl-3-x64.dll libcrypto-3-x64.dll zlib.dll; do
    if [[ -f "$DIR/$bad" ]]; then
      echo "error: package must not ship runtime dep $bad (deps are statically merged)" >&2
      exit 1
    fi
  done
elif [[ "$PLATFORM" == "android" ]]; then
  [[ -f "$DIR/libc++_shared.so" ]] || {
    echo "error: Android 包缺 libc++_shared.so（tdjson 现在是动态依赖它）" >&2
    ls -la "$DIR" >&2 || true
    exit 1
  }
  # libcrypto / libssl 的 SONAME 带版本号（libcrypto.so.3），按前缀找。
  compgen -G "$DIR/libcrypto.so*" >/dev/null || { echo "error: Android 包缺 libcrypto.so*" >&2; exit 1; }
  compgen -G "$DIR/libssl.so*" >/dev/null || { echo "error: Android 包缺 libssl.so*" >&2; exit 1; }
elif [[ "$PLATFORM" == "linux" ]]; then
  for need in 'libcrypto.so*' 'libssl.so*' 'libz.so*'; do
    compgen -G "$DIR/$need" >/dev/null || {
      echo "error: Linux 包缺 $need（tdjson 现在是动态依赖它；见 build-tdlib-linux.sh）" >&2
      ls -la "$DIR" >&2 || true
      exit 1
    }
  done
fi

echo "==> verifying $LIB"
python3 - "$LIB" "$PLATFORM" "$ARCH" <<'PY'
import json, struct, sys
from pathlib import Path

lib = Path(sys.argv[1])
platform = sys.argv[2]
arch = sys.argv[3]

data = lib.read_bytes()
if len(data) < 20:
    raise SystemExit(f"file too small: {lib}")

version_path = lib.parent / "VERSION.json"
meta = json.loads(version_path.read_text(encoding="utf-8"))

if meta.get("library") != lib.name:
    raise SystemExit(f"VERSION.json library mismatch: {meta.get('library')} vs {lib.name}")
if meta.get("platform") != platform:
    raise SystemExit(f"platform mismatch: {meta.get('platform')} vs {platform}")
if meta.get("arch") != arch:
    raise SystemExit(f"arch mismatch: {meta.get('arch')} vs {arch}")
# 链接形态按平台判定：
#   Android：linkage=shared，runtime_deps 非空（libc++_shared / libcrypto / libssl）；
#   Linux：linkage=shared，runtime_deps 非空（libcrypto / libssl / libz）；
#   其余桌面：linkage=static、runtime_deps 必须为空。
linkage = meta.get("linkage")
deps = meta.get("runtime_deps") or []
if platform in ("android", "linux"):
    if linkage != "shared":
        raise SystemExit(f"{platform} 期望 linkage=shared，实际 {linkage!r}")
    if not deps:
        raise SystemExit(f"{platform} 包的 runtime_deps 不能为空")
    for dep in deps:
        if not (lib.parent / dep).is_file():
            raise SystemExit(f"runtime_deps 里的 {dep} 不在包里")
    required = (
        ("libc++_shared.so", "libcrypto.so", "libssl.so")
        if platform == "android"
        else ("libcrypto.so", "libssl.so", "libz.so")
    )
    for prefix in required:
        if not any(dep.startswith(prefix) for dep in deps):
            raise SystemExit(f"{platform} 包缺 {prefix}*（runtime_deps={deps}）")
else:
    if linkage and linkage != "static":
        raise SystemExit(f"expected linkage=static, got {linkage!r}")
    if deps:
        raise SystemExit(f"runtime_deps should be empty after static merge: {deps}")

magic = data[:4]
if magic == b"\x7fELF":
    ei_class = data[4]
    ei_data = data[5]
    endianness = "<" if ei_data == 1 else ">"
    e_machine = struct.unpack(endianness + "H", data[18:20])[0]
    expected = {"x86_64": 62, "arm64": 183, "aarch64": 183, "x86": 3}.get(arch)
    if expected is not None and e_machine != expected:
        raise SystemExit(f"ELF e_machine mismatch: got {e_machine}, expected {expected} for {arch}")
    print(f"OK ELF class={ei_class} e_machine={e_machine}")
elif magic[:2] == b"MZ":
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew:e_lfanew+4] != b"PE\0\0":
        raise SystemExit("PE signature missing")
    machine = struct.unpack_from("<H", data, e_lfanew + 4)[0]
    expected = {"x86_64": 0x8664, "arm64": 0xAA64, "x86": 0x14C}.get(arch)
    if expected is not None and machine != expected:
        raise SystemExit(f"PE machine mismatch: got 0x{machine:04X}, expected 0x{expected:04X} for {arch}")
    print(f"OK PE machine=0x{machine:04X}")
elif magic in (
    b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe",
    b"\xfe\xed\xfa\xcf",
    b"\xfe\xed\xfa\xce",
    b"\xce\xfa\xed\xfe",
):
    if magic in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"):
        if magic == b"\xcf\xfa\xed\xfe":
            cputype = struct.unpack_from("<I", data, 4)[0]
        else:
            cputype = struct.unpack_from(">I", data, 4)[0]
        if arch == "x86_64" and cputype != 0x01000007:
            raise SystemExit(f"Mach-O cputype mismatch for x86_64: 0x{cputype:08X}")
        if arch in ("arm64", "aarch64") and cputype != 0x0100000C:
            raise SystemExit(f"Mach-O cputype mismatch for arm64: 0x{cputype:08X}")
        print(f"OK Mach-O cputype=0x{cputype:08X}")
    elif magic == b"\xca\xfe\xba\xbe":
        nfat = struct.unpack_from(">I", data, 4)[0]
        print(f"OK Mach-O FAT nfat_arch={nfat}")
        need = "x86_64" if arch == "x86_64" else "arm64"
        found_types = []
        off = 8
        for _ in range(nfat):
            cputype = struct.unpack_from(">I", data, off)[0]
            found_types.append(cputype)
            off += 20
        want = 0x01000007 if need == "x86_64" else 0x0100000C
        if want not in found_types:
            raise SystemExit(f"Mach-O FAT missing {need} cputype 0x{want:08X}; have {list(map(hex, found_types))}")
    else:
        print("OK Mach-O (32-bit or other)")
else:
    raise SystemExit(f"unknown binary magic: {data[:8].hex()}")

print(f"OK {platform}/{arch} package verified (linkage={meta.get('linkage')}): {lib}")
print(json.dumps(meta, indent=2))
PY
