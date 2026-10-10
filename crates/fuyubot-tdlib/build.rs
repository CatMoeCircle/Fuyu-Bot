//! vendored TDLib 的获取，以及 `td_api.tl` 在**编译期**的转换。
//!
//! - 原生库：只负责把它放到编译期能引用的位置并记录路径，**不负责加载**。
//!   `vendored` feature 未启用时这部分是纯空操作：不产生任何网络请求。
//! - `td_api.tl`：无论有没有 vendored 都会试着转换一次（来源是 `TDLIB_TD_API_TL` 或 vendored
//!   目录），产出两份文件——`$OUT_DIR/td_api.json`（运行时的元数据）与
//!   `$OUT_DIR/td_api_generated.rs`（Rust 类型），运行时分别用 `include_str!` / `include!` 烤进二进制。
//!   **转换只在编译期发生**，运行时不读文件、不解析、不生成。
//!
//! 任何失败都只打印警告、不 panic；schema 那条路始终会写出一个文件（失败时是 `{}`），
//! 因为运行时的 `include_str!` 指着它——少写一次就是编译错误。

#[allow(dead_code)] // 未启用 vendored 时这些函数用不上，但共享文件必须完整编译
#[path = "src/loader/vendored.rs"]
mod vendored;

// 与 src/schema.rs 共用同一份解析与 JSON 互转：编译期写盘、运行时读回，形状只能是同一个实现。
#[allow(dead_code)]
#[path = "src/tl_schema.rs"]
mod tl_schema;

// 从 td_api.tl 生成 Rust 类型（`src/td_api.rs` 用 include! 展出来）。
#[allow(dead_code)]
#[path = "src/tl_codegen.rs"]
mod tl_codegen;

use std::path::{Path, PathBuf};

/// `releases-repo` 没填真实值时用的占位符：下载必然 404，所以出错时要专门点出来。
const PLACEHOLDER_REPO: &str = "fuyubot/fuyubot";

#[derive(serde::Deserialize)]
struct Manifest {
    package: Package,
}

#[derive(serde::Deserialize)]
struct Package {
    metadata: Option<Metadata>,
}

#[derive(serde::Deserialize)]
struct Metadata {
    fuyubot: Option<Fuyubot>,
}

#[derive(serde::Deserialize, Default)]
struct Fuyubot {
    #[serde(rename = "tdlib-version")]
    tdlib_version: Option<String>,
    // 只有启用 vendored 时才会读它。
    #[cfg_attr(not(feature = "vendored"), allow(dead_code))]
    #[serde(rename = "releases-repo")]
    releases_repo: Option<String>,
}

fn main() {
    for key in [
        "TDLIB_TDJSON_PATH",
        vendored::SCHEMA_ENV,
        vendored::CACHE_ENV,
        vendored::REPO_ENV,
        "CARGO_FEATURE_VENDORED",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/loader/vendored.rs");
    // build.rs 与运行时共用这个文件，它变了要重新转换。
    println!("cargo:rerun-if-changed=src/tl_schema.rs");

    let fuyubot = read_metadata().unwrap_or_default();
    let version = fuyubot.tdlib_version.unwrap_or_else(|| "0.0.0".to_owned());
    // 运行时优先级 3 要拿它判断主版本兼容，所以无条件注入。
    println!("cargo:rustc-env=FUYUBOT_TDLIB_VERSION={version}");

    // td_api.tl 是**必需**输入：与 vendored 无关，拿不到就让构建失败（见 acquire_schema）。
    let schema = acquire_schema(&version, fuyubot.releases_repo.as_deref());

    // 原生库只有 vendored 才取；td_api.tl 不走这条路。
    if std::env::var_os("CARGO_FEATURE_VENDORED").is_some() {
        #[cfg(feature = "vendored")]
        match acquire(&version, fuyubot.releases_repo.as_deref()) {
            Ok(dir) => {
                println!(
                    "cargo:rustc-env=FUYUBOT_TDLIB_VENDORED_DIR={}",
                    dir.display()
                );
                println!("cargo:rustc-env=FUYUBOT_TDLIB_VENDORED_VERSION={version}");
            }
            Err(reason) => println!("cargo:warning=vendored TDLib 没能就位：{reason}"),
        }
        // 启用了 feature 却没拿到 cfg，说明构建脚本的 cfg 传播出了问题，明说而不是静默跳过。
        #[cfg(not(feature = "vendored"))]
        println!("cargo:warning=启用了 vendored feature，但构建脚本没有拿到 feature cfg");
    }

    bake_schema(&schema);
}

/// **无条件**把 `td_api.tl` 弄到手——它不属于 vendored 那一套，是构建的必需输入。
///
/// 顺序：`TDLIB_TD_API_TL`（显式指定）→ 缓存 → 从 Releases 下载。
/// 下载成功会写进缓存，所以一台机器只会真正下载一次。
///
/// **拿不到就让构建失败**：不存在「没有就不转换」这条路。
fn acquire_schema(version: &str, configured_repo: Option<&str>) -> PathBuf {
    if let Some(path) = explicit_schema() {
        return path;
    }

    let cached = vendored::cached_schema(&vendored::cache_root(), version);
    if cached.is_file() {
        println!(
            "cargo:warning=复用缓存里的 {}（{}）",
            vendored::SCHEMA_ASSET,
            cached.display()
        );
        return cached;
    }

    let Some(repo) = resolve_repo(configured_repo) else {
        panic!("{}", missing_repo_hint());
    };
    let url = vendored::release_url(&repo, version, vendored::SCHEMA_ASSET);
    println!(
        "cargo:warning=下载 TDLib {version} 的 {}：{url}",
        vendored::SCHEMA_ASSET
    );
    let bytes = http_get(&url)
        .unwrap_or_else(|error| panic!("{}", download_hint(&url, &error, &cached, &repo)));

    if let Some(dir) = cached.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("创建 {} 失败：{e}", dir.display()));
    }
    std::fs::write(&cached, &bytes).unwrap_or_else(|e| panic!("写 {} 失败：{e}", cached.display()));
    println!("cargo:warning=已缓存到 {}", cached.display());
    cached
}

/// `TDLIB_TD_API_TL` 指定的那份（文件或目录）。
///
/// 它优先级最高：是离线环境、以及「用自己那份 schema」的出口。
/// **相对路径会按「as-given → crate 根 → workspace 根」依次试**：build.rs 的 CWD 是
/// crate 根目录，而人多半是在 workspace 根敲命令，两边都得认。
fn explicit_schema() -> Option<PathBuf> {
    let given = std::env::var_os(vendored::SCHEMA_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    let given = if given.is_dir() {
        given.join(vendored::SCHEMA_ASSET)
    } else {
        given
    };

    let mut candidates = vec![given.clone()];
    if given.is_relative() {
        if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
            let manifest = PathBuf::from(manifest);
            if let Some(workspace) = manifest.parent().and_then(Path::parent) {
                candidates.push(workspace.join(&given));
            }
            candidates.push(manifest.join(&given));
        }
    }

    let mut tried = Vec::new();
    for candidate in &candidates {
        if candidate.is_file() {
            // 用户自己那份改了要重新转换。
            println!("cargo:rerun-if-changed={}", candidate.display());
            return Some(candidate.clone());
        }
        tried.push(candidate.display().to_string());
    }
    panic!(
        "{} 指向的 {} 不存在（拿不到 td_api.tl 就没法生成 TDLib 类型）。\n试过：{tried:?}",
        vendored::SCHEMA_ENV,
        given.display()
    );
}

/// Releases 仓库：环境变量优先，其次 `[package.metadata.fuyubot] releases-repo`。
fn resolve_repo(configured: Option<&str>) -> Option<String> {
    std::env::var(vendored::REPO_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            configured
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn missing_repo_hint() -> String {
    format!(
        "没配 Releases 仓库，拿不到 {}。\n\
         它是构建的必需输入（要把 TDLib API 转成 Rust 类型），三个办法：\n\
         ① 在 crates/fuyubot-tdlib/Cargo.toml 的 [package.metadata.fuyubot] 里写真实的 releases-repo；\n\
         ② 设 {} 覆盖它；\n\
         ③ 设 {} 指向本地那份 {}。",
        vendored::SCHEMA_ASSET,
        vendored::REPO_ENV,
        vendored::SCHEMA_ENV,
        vendored::SCHEMA_ASSET
    )
}

fn download_hint(url: &str, error: &str, cached: &Path, repo: &str) -> String {
    let placeholder = if repo == PLACEHOLDER_REPO {
        format!(
            "\n注意：releases-repo 现在还是占位符 `{PLACEHOLDER_REPO}`，下载必然 404。\
             release 资产就位之后把它改成真实的 GitHub owner/repo。"
        )
    } else {
        String::new()
    };
    format!(
        "下载 {url} 失败：{error}{placeholder}\n\
         拿不到 {} 就没法生成 TDLib 类型，而它是构建的必需输入。\n\
         ① 确认 {repo} 的 release 里有这个资产，标签形如 `TDLib V1.8.66`；\n\
         ② 或者设 {} 指向本地那份；\n\
         ③ 或者手动把它放到 {}。",
        vendored::SCHEMA_ASSET,
        vendored::SCHEMA_ENV,
        cached.display()
    )
}

/// 在**编译期**把 `td_api.tl` 转成两份产物：
///
/// - `$OUT_DIR/td_api.json` —— 运行时的元数据（`TdApiSchema::compiled` 用）；
/// - `$OUT_DIR/td_api_generated.rs` —— Rust 类型（`td_api` 模块用 `include!` 展出来）。
///
/// 读不了、解析不了、没有分区标记，全都是**构建失败**：转换是必需的一步，不是尽力而为。
fn bake_schema(schema_path: &Path) {
    let out_dir = std::env::var("OUT_DIR")
        .unwrap_or_else(|_| panic!("OUT_DIR 未设置——构建脚本不该在没有它的环境里运行"));
    let out_dir = PathBuf::from(out_dir);

    let text = std::fs::read_to_string(schema_path)
        .unwrap_or_else(|e| panic!("读 {} 失败：{e}", schema_path.display()));
    let entries = tl_schema::parse(&text)
        .unwrap_or_else(|e| panic!("解析 {} 失败：{e}", schema_path.display()));

    let functions = entries
        .iter()
        .filter(|entry| entry.kind == tl_schema::EntryKind::Function)
        .count();
    let constructors = entries
        .iter()
        .filter(|entry| entry.kind == tl_schema::EntryKind::Type)
        .count();
    if functions == 0 || constructors == 0 {
        panic!(
            "{} 里没有 ---functions--- / ---types--- 分区标记（函数 {functions} 个、构造器 {constructors} 个）。\n\
             分不开函数与构造器就没法生成类型，而这件事没有可靠的启发式可退：\
             `updateNewMessage ... = Update` 是构造器，但首字母大写之后是 `UpdateNewMessage` ≠ `Update`。",
            schema_path.display()
        );
    }

    println!(
        "cargo:warning=已转换 {}：共 {} 条（函数 {functions}、构造器 {constructors}、未归类 {}），\
         元数据与 Rust 类型都已烤进二进制",
        schema_path.display(),
        entries.len(),
        entries.len() - functions - constructors
    );

    write_json(
        &out_dir.join("td_api.json"),
        &tl_schema::to_json(&entries, Some(&schema_path.display().to_string())),
    );
    write_text(
        &out_dir.join("td_api_generated.rs"),
        &tl_codegen::generate(&entries),
    );
}

fn write_json(dest: &Path, value: &serde_json::Value) {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned());
    write_text(dest, &text);
}

fn write_text(dest: &Path, text: &str) {
    if let Err(e) = std::fs::write(dest, text) {
        // 写不成的话运行时的 include_str!/include! 会编译不过，把原因说清楚。
        println!(
            "cargo:warning=写 {} 失败：{e}（运行时的 include! 会因此编译失败）",
            dest.display()
        );
    }
}

/// 读 `[package.metadata.fuyubot]`；读不到就当没配。
fn read_metadata() -> Option<Fuyubot> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let text = std::fs::read_to_string(Path::new(&manifest).join("Cargo.toml")).ok()?;
    let parsed: Manifest = toml::from_str(&text).ok()?;
    parsed.package.metadata?.fuyubot
}

/// 把当前平台的库和 `td_api.tl` 都搞到 `$OUT_DIR/vendored/`，返回该目录。
///
/// `td_api.tl` 是平台无关的 API 定义文件，跟动态库发布在同一个 release 里；
/// 运行时靠 `fuyubot_tdlib::schema` 把它转成可校验的元数据。
#[cfg(feature = "vendored")]
fn acquire(version: &str, releases_repo: Option<&str>) -> Result<std::path::PathBuf, String> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let lib_name = vendored::library_filename(&os)
        .ok_or_else(|| format!("平台 {os}/{arch} 没有约定的动态库文件名"))?;

    let out_dir = std::env::var("OUT_DIR").map_err(|_| "OUT_DIR 未设置".to_owned())?;
    let vendored_dir = std::path::PathBuf::from(out_dir).join("vendored");
    let root = vendored::cache_root();

    let library_asset = vendored::asset_name(&os, &arch)
        .ok_or_else(|| format!("平台 {os}/{arch} 在 Releases 里没有对应资产"))?;
    let cached_library = vendored::cached_library(&root, version, &os, &arch);

    // 只有真需要下载时才要求配仓库：缓存命中时不该因为没配 repo 就失败。
    let repo = if cached_library.is_file() {
        None
    } else {
        Some(resolve_repo(releases_repo).ok_or_else(|| {
            format!(
                "没配 Releases 仓库：请在 [package.metadata.fuyubot] 里写 releases-repo，或设置 {}",
                vendored::REPO_ENV
            )
        })?)
    };

    // 动态库：可能是裸库或 tar.gz，交给 extract_library 认。
    // （td_api.tl 不在这儿取：它由 acquire_schema 单独负责，与 vendored 无关。）
    fetch_cached(
        repo.as_deref(),
        version,
        library_asset,
        &cached_library,
        true,
        &os,
    )?;

    std::fs::create_dir_all(&vendored_dir)
        .map_err(|e| format!("创建 {} 失败: {e}", vendored_dir.display()))?;
    let dest = vendored_dir.join(lib_name);
    std::fs::copy(&cached_library, &dest).map_err(|e| {
        format!(
            "把 {} 复制到 {} 失败: {e}",
            cached_library.display(),
            dest.display()
        )
    })?;
    Ok(vendored_dir)
}

/// 缓存里没有就从 Releases 取回来。`unpack` 为真时按裸库/tar.gz 处理，否则原样存。
#[cfg(feature = "vendored")]
fn fetch_cached(
    repo: Option<&str>,
    version: &str,
    asset: &str,
    dest: &std::path::Path,
    unpack: bool,
    os: &str,
) -> Result<(), String> {
    if dest.is_file() {
        println!("cargo:warning=复用缓存里的 {asset}（{}）", dest.display());
        return Ok(());
    }
    let repo = repo.ok_or_else(|| format!("缓存里缺 {asset}，但没有可用的 Releases 仓库"))?;

    let url = vendored::release_url(repo, version, asset);
    println!("cargo:warning=下载 TDLib {version} 的 {asset}：{url}");
    let bytes = http_get(&url).map_err(|e| format!("下载 {url} 失败: {e}"))?;

    match http_get(&format!("{url}.sha256")) {
        Ok(digest) => {
            let text = String::from_utf8_lossy(&digest);
            let expected = text
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let actual = vendored::sha256_hex(&bytes);
            if expected != actual {
                return Err(format!(
                    "{asset} 的 SHA256 不匹配：期望 {expected}，实际 {actual}"
                ));
            }
        }
        Err(e) => println!("cargo:warning=没拿到 {url}.sha256（{e}），跳过校验"),
    }

    let payload = if unpack {
        vendored::extract_library(&bytes, os)?
    } else {
        bytes
    };
    let dir = dest
        .parent()
        .ok_or_else(|| format!("缓存路径 {} 没有父目录", dest.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    std::fs::write(dest, &payload).map_err(|e| format!("写入 {} 失败: {e}", dest.display()))?;
    Ok(())
}

/// 构建期的 HTTP GET。
///
/// **不按 `vendored` 门控**：`td_api.tl` 是必需输入，无论开不开 vendored 都要下载它。
/// `ureq` 因此是常开的 build-dependency（只在构建期，不进运行时的依赖图）。
fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::get(url).call().map_err(|e| e.to_string())?;
    response
        .into_body()
        .read_to_vec()
        .map_err(|e| e.to_string())
}
