#!/usr/bin/env bash
# Package tdjson artifacts into a versioned archive and write VERSION.json.
# Args: <tdjson-dir> <output-archive-path> <platform> <arch> <tdlib-version> <tdlib-commit>
set -euo pipefail

TDLIB_JSON_DIR="${1:?tdjson dir}"
ARCHIVE_PATH="${2:?archive path}"
PLATFORM="${3:?platform}"
ARCH="${4:?arch}"
TDLIB_VERSION="${5:?tdlib version}"
TDLIB_COMMIT="${6:?tdlib commit}"

if [[ ! -d "$TDLIB_JSON_DIR" ]]; then
  echo "error: missing tdjson dir $TDLIB_JSON_DIR" >&2
  exit 1
fi

LIB_NAME=""
for name in libtdjson.so libtdjson.dylib tdjson.dll; do
  if [[ -f "$TDLIB_JSON_DIR/$name" ]]; then
    LIB_NAME="$name"
    break
  fi
done
if [[ -z "$LIB_NAME" ]]; then
  echo "error: tdjson library not found in $TDLIB_JSON_DIR" >&2
  ls -la "$TDLIB_JSON_DIR" >&2 || true
  exit 1
fi

# Use stable, platform-specific directory names inside each archive.
# Keep the outer archive filenames versioned for release compatibility.
case "${PLATFORM}:${ARCH}" in
  linux:x86_64) PKG_NAME="TDLib-x86_64-linux" ;;
  linux:arm64|linux:aarch64) PKG_NAME="TDLib-aarch64-linux" ;;
  macos:arm64|macos:aarch64) PKG_NAME="TDLib-macos-arm64" ;;
  windows:x86_64) PKG_NAME="TDLib-windows-x86_64" ;;
  windows:arm64|windows:aarch64) PKG_NAME="TDLib-windows-arm64" ;;
  *)
    echo "unsupported platform/architecture: ${PLATFORM}/${ARCH}" >&2
    exit 1
    ;;
esac

BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

if command -v cygpath >/dev/null 2>&1; then
  ARCHIVE_PATH="$(cygpath -m "$ARCHIVE_PATH")"
fi
mkdir -p "$(dirname "$ARCHIVE_PATH")"

STAGE_ROOT="$(mktemp -d)"
STAGE_DIR="$STAGE_ROOT/$PKG_NAME"
mkdir -p "$STAGE_DIR"

# Ship ONLY the tdjson binary (+ optional import lib). OpenSSL/zlib are merged in.
cp -a "$TDLIB_JSON_DIR/$LIB_NAME" "$STAGE_DIR/"
if [[ -f "$TDLIB_JSON_DIR/tdjson.lib" ]]; then
  cp -a "$TDLIB_JSON_DIR/tdjson.lib" "$STAGE_DIR/"
fi
if [[ -f "$TDLIB_JSON_DIR/build-info.txt" ]]; then
  cp -a "$TDLIB_JSON_DIR/build-info.txt" "$STAGE_DIR/"
fi
if [[ -f "$TDLIB_JSON_DIR/VERSION.json" ]]; then
  # Keep any pre-written meta; we rewrite a canonical VERSION.json below.
  true
fi

# Linkage note from build-info (if present)
LINKAGE="static"
if [[ -f "$STAGE_DIR/build-info.txt" ]]; then
  if grep -q '^linkage=' "$STAGE_DIR/build-info.txt" 2>/dev/null; then
    LINKAGE="$(grep -m1 '^linkage=' "$STAGE_DIR/build-info.txt" | cut -d= -f2- | tr -d '\r')"
  fi
fi

# Optional non-runtime companion (import lib for MSVC link-time consumers).
LINK_LIB_JSON="null"
if [[ -f "$STAGE_DIR/tdjson.lib" ]]; then
  LINK_LIB_JSON="\"tdjson.lib\""
fi

cat > "$STAGE_DIR/VERSION.json" <<EOF
{
  "tdlib_version": "${TDLIB_VERSION}",
  "tdlib_commit": "${TDLIB_COMMIT}",
  "platform": "${PLATFORM}",
  "arch": "${ARCH}",
  "library": "${LIB_NAME}",
  "linkage": "${LINKAGE}",
  "linkage_note": "OpenSSL/zlib are statically linked into ${LIB_NAME}",
  "import_lib": ${LINK_LIB_JSON},
  "runtime_deps": [],
  "build_date_utc": "${BUILD_DATE}",
  "consumed_by": "fuyutd (dynamic load via libloading)"
}
EOF

cat > "$STAGE_DIR/README.md" <<EOF
# TDLib binary package

- **Platform**: ${PLATFORM}
- **Arch**: ${ARCH}
- **TDLib version**: ${TDLIB_VERSION}
- **TDLib commit**: ${TDLIB_COMMIT}
- **Library**: \`${LIB_NAME}\`
- **Linkage**: ${LINKAGE}
- **Built (UTC)**: ${BUILD_DATE}

## Static merge

OpenSSL and zlib are **statically linked into** \`${LIB_NAME}\`.
You do **not** need to ship `libssl` / `libcrypto` / `libz` next to the library.

This follows the same approach as [eilvelia/tdl prebuilt-tdlib](https://github.com/eilvelia/tdl/blob/main/.github/workflows/prebuilt-tdlib.yml)
(`OPENSSL_USE_STATIC_LIBS=TRUE`, `ZLIB_USE_STATIC_LIBS=TRUE`, static vcpkg triplets on Windows).

## Usage with fuyutd

```rust
use fuyutd::Client;

let client = Client::builder()
    .tdjson("${LIB_NAME}")
    .libdir("./tdlib")
    .verbosity_level(1)
    .build()?;
```

## Layout

\`\`\`
${PKG_NAME}/
  ${LIB_NAME}
  VERSION.json
  README.md
\`\`\`

## Notes

- Linux: \`libtdjson.so\` carries OpenSSL/zlib inside; system libc/libstdc++ still apply.
- macOS: \`libtdjson.dylib\` linked against static OpenSSL/zlib; \`@rpath\` id + ad-hoc codesign applied.
- Windows: \`tdjson.dll\` built with vcpkg **static** triplets (\`*-windows-static\`); OpenSSL/zlib merged in. Optional \`tdjson.lib\` is import-lib only (not a runtime dep).
EOF

case "$ARCHIVE_PATH" in
  *.tar.gz|*.tgz)
    tar -czf "$ARCHIVE_PATH" -C "$STAGE_ROOT" "$PKG_NAME"
    ;;
  *.zip)
    if command -v zip >/dev/null 2>&1; then
      (cd "$STAGE_ROOT" && zip -qr "$ARCHIVE_PATH" "$PKG_NAME")
    else
      python3 - "$STAGE_ROOT" "$PKG_NAME" "$ARCHIVE_PATH" <<'PY'
import os, sys, zipfile
stage_root, pkg, archive = sys.argv[1], sys.argv[2], sys.argv[3]
base = os.path.join(stage_root, pkg)
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zf:
    for root, _, files in os.walk(base):
        for f in files:
            full = os.path.join(root, f)
            rel = os.path.relpath(full, stage_root)
            zf.write(full, rel.replace(os.sep, "/"))
PY
    fi
    ;;
  *)
    echo "unsupported archive type: $ARCHIVE_PATH" >&2
    exit 1
    ;;
esac

rm -rf "$STAGE_ROOT"
echo "==> packaged $ARCHIVE_PATH (static-linked tdjson only)"
ls -lh "$ARCHIVE_PATH"
