//! vendored 原生库的资产映射、缓存布局与校验。
//!
//! 这个文件**同时**被 `build.rs`（`#[path]` 引入）和库本体编译，
//! 所以它只能依赖 std 和可选依赖，不得引用 crate 内其他模块。

use std::path::{Path, PathBuf};

/// Releases 资产名 → 平台动态库文件名。
pub const ASSET_LINUX_X86_64: &str = "TDLib-x86_64-linux";
pub const ASSET_LINUX_AARCH64: &str = "TDLib-aarch64-linux";
pub const ASSET_MACOS_ARM64: &str = "TDLib-macos-arm64";
pub const ASSET_WINDOWS_X86_64: &str = "TDLib-windows-x86_64";

/// 缓存根目录环境变量。
pub const CACHE_ENV: &str = "FUYUBOT_TDLIB_VENDORED_CACHE";
/// 覆盖 `[package.metadata.fuyubot] releases-repo` 的环境变量。
pub const REPO_ENV: &str = "FUYUBOT_TDLIB_RELEASES_REPO";

/// TDLib 的 API 定义文件，跟着动态库一起发布在同一个 release 里。
///
/// 它是**平台无关**的，所以缓存路径不带 `os-arch` 那一段。
pub const SCHEMA_ASSET: &str = "td_api.tl";
/// 覆盖 `td_api.tl` 位置的环境变量（不配就用 vendored / 库旁边那份）。
pub const SCHEMA_ENV: &str = "TDLIB_TD_API_TL";

/// 平台动态库文件名；不支持的平台返回 `None`。
pub fn library_filename(os: &str) -> Option<&'static str> {
    match os {
        "windows" => Some("tdjson.dll"),
        "macos" => Some("libtdjson.dylib"),
        "linux" => Some("libtdjson.so"),
        _ => None,
    }
}

/// `(os, arch)` → Releases 资产名。
pub fn asset_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some(ASSET_LINUX_X86_64),
        ("linux", "aarch64") => Some(ASSET_LINUX_AARCH64),
        ("macos", "aarch64") => Some(ASSET_MACOS_ARM64),
        ("windows", "x86_64") => Some(ASSET_WINDOWS_X86_64),
        _ => None,
    }
}

/// 文件名是否像某个平台的 tdjson。
pub fn is_library_file(name: &str, os: &str) -> bool {
    match os {
        "windows" => name.starts_with("tdjson") && name.ends_with(".dll"),
        "macos" => name.starts_with("libtdjson") && name.contains(".dylib"),
        _ => name.starts_with("libtdjson") && name.contains(".so"),
    }
}

/// 缓存根目录：环境变量优先，其次 `$CARGO_HOME/fuyubot-tdlib-vendored`。
pub fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os(CACHE_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"));
    cargo_home.join("fuyubot-tdlib-vendored")
}

/// 某个版本 + 平台在缓存里的目录：`<root>/<version>/<os>-<arch>`。
pub fn cache_dir(root: &Path, version: &str, os: &str, arch: &str) -> PathBuf {
    root.join(version).join(format!("{os}-{arch}"))
}

/// 缓存里该平台库文件的完整路径。
pub fn cached_library(root: &Path, version: &str, os: &str, arch: &str) -> PathBuf {
    cache_dir(root, version, os, arch).join(library_filename(os).unwrap_or("tdjson"))
}

/// 缓存里 `td_api.tl` 的路径：`<root>/<version>/td_api.tl`（与平台无关）。
pub fn cached_schema(root: &Path, version: &str) -> PathBuf {
    root.join(version).join(SCHEMA_ASSET)
}

/// Releases 下载 URL。Tag 是 `TDLib V{version}`，用户名里必须带空格，所以做百分号编码。
pub fn release_url(repo: &str, version: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/TDLib%20V{version}/{asset}")
}

/// SHA256 十六进制小写。
#[cfg(feature = "vendored")]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 从下载到的字节里取出裸库文件。
///
/// 支持两种资产形态：裸动态库，和 `.tar.gz`（取其中第一个匹配平台的库文件）。
#[cfg(feature = "vendored")]
pub fn extract_library(bytes: &[u8], os: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;

    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| format!("读取 tar.gz 失败: {e}"))?;
    let mut fallback: Option<Vec<u8>> = None;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("读取 tar.gz 条目失败: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("解压条目 {name} 失败: {e}"))?;
        if is_library_file(&name, os) {
            return Ok(buf);
        }
        fallback.get_or_insert(buf);
    }
    fallback.ok_or_else(|| "tar.gz 里没有任何普通文件".to_owned())
}

/// 解析 `libtdjson.so.<major>[.<minor>[.<patch>]]`，缺失的部分补 0。
pub fn parse_versioned_library_name(name: &str) -> Option<(u32, u32, u32)> {
    let rest = name.strip_prefix("libtdjson.so.")?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    // 多于三段的不认（例如 libtdjson.so.1.8.66.debug）
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// `"1.8.66"` → `(1, 8, 66)`；解析失败返回 `None`。
pub fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_and_library_names() {
        assert_eq!(asset_name("linux", "x86_64"), Some("TDLib-x86_64-linux"));
        assert_eq!(asset_name("linux", "aarch64"), Some("TDLib-aarch64-linux"));
        assert_eq!(asset_name("macos", "aarch64"), Some("TDLib-macos-arm64"));
        assert_eq!(
            asset_name("windows", "x86_64"),
            Some("TDLib-windows-x86_64")
        );
        assert_eq!(asset_name("windows", "aarch64"), None);

        assert_eq!(library_filename("windows"), Some("tdjson.dll"));
        assert_eq!(library_filename("macos"), Some("libtdjson.dylib"));
        assert_eq!(library_filename("linux"), Some("libtdjson.so"));
        assert_eq!(library_filename("freebsd"), None);
    }

    #[test]
    fn cache_layout_is_version_scoped() {
        let root = Path::new("/cache");
        assert_eq!(
            cache_dir(root, "1.8.66", "windows", "x86_64"),
            Path::new("/cache/1.8.66/windows-x86_64")
        );
        assert_eq!(
            cached_library(root, "1.8.66", "linux", "x86_64"),
            Path::new("/cache/1.8.66/linux-x86_64/libtdjson.so")
        );
        // td_api.tl 与平台无关，不带 os-arch 那一段
        assert_eq!(
            cached_schema(root, "1.8.66"),
            Path::new("/cache/1.8.66/td_api.tl")
        );
    }

    #[test]
    fn release_url_encodes_the_space_in_the_tag() {
        assert_eq!(
            release_url("fuyubot/fuyubot", "1.8.66", "TDLib-windows-x86_64"),
            "https://github.com/fuyubot/fuyubot/releases/download/TDLib%20V1.8.66/TDLib-windows-x86_64"
        );
    }

    #[test]
    fn versioned_library_names() {
        assert_eq!(
            parse_versioned_library_name("libtdjson.so.1.8.66"),
            Some((1, 8, 66))
        );
        assert_eq!(
            parse_versioned_library_name("libtdjson.so.1.8"),
            Some((1, 8, 0))
        );
        assert_eq!(
            parse_versioned_library_name("libtdjson.so.1"),
            Some((1, 0, 0))
        );
        assert_eq!(parse_versioned_library_name("libtdjson.so"), None);
        assert_eq!(parse_versioned_library_name("libtdjson.so.1.8.66.1"), None);
        assert_eq!(parse_version("1.8.66"), Some((1, 8, 66)));
        assert_eq!(parse_version("2"), Some((2, 0, 0)));
    }

    #[cfg(feature = "vendored")]
    #[test]
    fn sha256_and_extract() {
        // 空输入的 SHA256 是众所周知的常量。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        // 裸库：原样返回。
        let raw = b"\x7fELF fake tdjson";
        assert_eq!(extract_library(raw, "linux").unwrap(), raw);

        // tar.gz：挑出匹配平台的库文件，忽略其它文件。
        let mut tar_bytes = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            let files: [(&str, &[u8]); 2] = [
                ("README.md", b"not a library"),
                ("libtdjson.so.1.8.66", b"real library bytes"),
            ];
            for (path, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, path, data).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(
            extract_library(&tar_bytes, "linux").unwrap(),
            b"real library bytes"
        );
    }
}
