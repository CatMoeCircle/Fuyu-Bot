#!/usr/bin/env bash
# 把编好的内置插件铺成**插件分发格式 v1 的目录包**：
#
#   <dest>/<id>/plugin.json
#   <dest>/<id>/bin/<stem>.<ext>
#
# 元数据只有一份：仓库根的 `plugins/builtins.json`（`fuyubot init` / `run` 里的
# `scaffold::install_builtins` 读的也是它）。版本取根 `Cargo.toml` 的
# `[workspace.package].version`，与三个插件 crate 的 `version.workspace = true` 同源。
#
# 用法：
#   builtin-bundles.sh <dest-plugins-dir> <artifact-dir> <ext> [required|optional]
#
#   <ext>       dll | so | dylib（目标平台，不是宿主的）
#   required    缺任一产物就 exit 1（可执行文件包 / Android 部署包）
#   optional    缺的只 warn 并跳过（JNI 包：内置插件本来就是可选的）
#
# 注意：cargo 在 Linux / macOS 上给 cdylib 加 `lib` 前缀，产物目录里两种名字都认；
# 包里一律写**无前缀**的 `<stem>.<ext>`（清单的 entry 也按它写）。
set -euo pipefail

DEST="${1:?dest plugins dir}"
SRC="${2:?artifact dir}"
EXT="${3:?library extension: dll | so | dylib}"
MODE="${4:-required}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
META="$ROOT/plugins/builtins.json"
CARGO_TOML="$ROOT/Cargo.toml"
ABI_LIB="$ROOT/crates/fuyubot-abi/src/lib.rs"

[[ -f "$META" ]] || { echo "error: 找不到内置插件元数据 $META" >&2; exit 1; }
[[ -f "$CARGO_TOML" ]] || { echo "error: 找不到 $CARGO_TOML" >&2; exit 1; }
[[ -f "$ABI_LIB" ]] || { echo "error: 找不到 $ABI_LIB" >&2; exit 1; }
[[ -d "$SRC" ]] || { echo "error: 产物目录 $SRC 不存在" >&2; exit 1; }

# 找一个真能用的 python3（Windows 上 `python3` 有时只是应用商店的转发壳）。
PY="${PYTHON:-}"
if [[ -z "$PY" ]]; then
  for candidate in python3 python; do
    if [[ "$("$candidate" -c 'import sys; print(sys.version_info[0])' 2>/dev/null || true)" == "3" ]]; then
      PY="$candidate"
      break
    fi
  done
fi
[[ -n "$PY" ]] || { echo "error: 找不到可用的 python3（铺内置插件包需要它）" >&2; exit 1; }

# Windows 控制台默认 cp936：不钉 UTF-8 的话中文警告会变成乱码（GitHub runner 上是 UTF-8）。
export PYTHONIOENCODING=utf-8

mkdir -p "$DEST"

"$PY" - "$META" "$CARGO_TOML" "$ABI_LIB" "$DEST" "$SRC" "$EXT" "$MODE" <<'PY'
import json
import re
import shutil
import sys
from pathlib import Path

meta_path, cargo_toml, abi_lib, dest, src, ext, mode = sys.argv[1:8]
dest = Path(dest)
src = Path(src)

# `[workspace.package]` 段里的 version（成员都 `version.workspace = true`）。
text = Path(cargo_toml).read_text(encoding="utf-8")
section = re.search(r"^\[workspace\.package\]\s*$", text, re.M)
if not section:
    raise SystemExit(f"error: {cargo_toml} 里没有 [workspace.package]")
version = re.search(r'^version\s*=\s*"([^"]+)"', text[section.end():], re.M)
if not version:
    raise SystemExit(f"error: {cargo_toml} 的 [workspace.package] 里没有 version")
version = version.group(1)

# ABI 版本以 fuyubot-abi 常量为准（与运行时 `install_builtins` 写进清单的值同源）。
abi = re.search(
    r"pub const ABI_VERSION:\s*u32\s*=\s*(\d+);",
    Path(abi_lib).read_text(encoding="utf-8"),
)
if not abi:
    raise SystemExit(f"error: {abi_lib} 里找不到 ABI_VERSION")
abi = int(abi.group(1))

plugins = json.loads(Path(meta_path).read_text(encoding="utf-8"))
for plugin in plugins:
    plugin_id = plugin["id"]
    stem = plugin["stem"]
    artifact = src / f"{stem}.{ext}"
    if not artifact.is_file():
        prefixed = src / f"lib{stem}.{ext}"
        artifact = prefixed if prefixed.is_file() else artifact
    if not artifact.is_file():
        message = f"缺内置插件产物 {stem}.{ext}（带 lib 前缀的也试过了，目录 {src}）"
        if mode == "optional":
            print(f"warning: {message}，本次不带 {plugin_id}", file=sys.stderr)
            continue
        raise SystemExit(f"error: {message}")

    bundle = dest / plugin_id
    bin_dir = bundle / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(artifact, bin_dir / f"{stem}.{ext}")

    # 字段顺序与 `BundleManifest::to_json()` 一致，与运行时生成的清单同形。
    # `abi` 只是静态预检用的建议值，真值仍以 `plugin_init` 返回的 handle 为准。
    manifest = {
        "schema": 1,
        "id": plugin_id,
        "name": plugin["name"],
        "version": version,
        "kind": "native",
        "entry": f"bin/{stem}.{ext}",
        "abi": abi,
        "platforms": [],
        "depends": [],
        "optional_depends": [],
        "provides": [],
        "description": plugin["description"],
        "license": "MIT OR Apache-2.0",
        "homepage": "",
    }
    (bundle / "plugin.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(f"  packaged {plugin_id} -> {bundle}")
PY
