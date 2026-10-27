//! vendored 原生库的获取。
//!
//! - 原生库：负责把它放到编译期能引用的位置并记录路径，**并另拷一份到可执行文件旁边**
//!   （那是「把二进制拷走也能跑」的部署形态，见 [`stage_next_to_binary`]），**不负责加载**。
//!   `vendored` feature 未启用时不会产生任何网络请求。
//! - `td_api.tl` 的获取与转换**不在这里**——那是 `fuyubot-tdlib-sys/build.rs` 的事
//!   （类型与元数据都住在 sys，见那份构建脚本的模块文档）。
//!
//! 缓存与下载设施（缓存根、`http_get`、`read_metadata`…）与 sys 的构建脚本
//! 共用 `../fuyubot-tdlib-sys/src/build_support.rs` 这一份实现——schema 与动态库
//! 必须落进**同一串**缓存根，两份实现会漂（见那份文件的模块文档）。

#[allow(dead_code)] // 未启用 vendored 时这些函数用不上，但共享文件必须完整编译
#[path = "src/loader/vendored.rs"]
mod vendored;

// 与 fuyubot-tdlib-sys/build.rs 共用的缓存与下载设施（跨 crate 的同一份文件）。
#[allow(dead_code)]
#[path = "../fuyubot-tdlib-sys/src/build_support.rs"]
mod build_support;

use std::path::{Path, PathBuf};

fn main() {
    for key in [
        "TDLIB_TDJSON_PATH",
        build_support::CACHE_ENV,
        build_support::REPO_ENV,
        "CARGO_FEATURE_VENDORED",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    // `tdlib-version` 在本清单里：不盯它就没法在换版本后重新解析 vendored 布局。
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src/loader/vendored.rs");
    println!("cargo:rerun-if-changed=../fuyubot-tdlib-sys/src/build_support.rs");

    let fuyubot = build_support::read_metadata().unwrap_or_default();
    let version = fuyubot.tdlib_version.unwrap_or_else(|| "0.0.0".to_owned());
    // 运行时优先级 3 要拿它判断主版本兼容，所以无条件注入。
    println!("cargo:rustc-env=FUYUBOT_TDLIB_VERSION={version}");

    // 原生库只有 vendored 才取；`td_api.tl` 与这条 feature 无关（sys 那侧无条件转换）。
    let vendored_dir = acquire_vendored(&version, fuyubot.releases_repo.as_deref());
    // 再放一份到**可执行文件旁边**：编译期烤进去的 vendored 路径只对构建机有效（gaps 缺口 3）。
    stage_next_to_binary(vendored_dir.as_deref(), &version);
}

/// vendored 动态库就位（未启用 feature 时是纯空操作），返回 `$OUT_DIR/vendored`。
///
/// 拆成两层是为了让 `#[cfg]` 只包住真正依赖 feature 的那一半：否则未启用 feature 时
/// 那两个参数会被判成未使用（这个构建脚本要求 0 warning）。
fn acquire_vendored(version: &str, releases_repo: Option<&str>) -> Option<PathBuf> {
    if std::env::var_os("CARGO_FEATURE_VENDORED").is_none() {
        return None;
    }
    acquire_vendored_enabled(version, releases_repo)
}

#[cfg(feature = "vendored")]
fn acquire_vendored_enabled(version: &str, releases_repo: Option<&str>) -> Option<PathBuf> {
    match acquire(version, releases_repo) {
        Ok(dir) => {
            println!(
                "cargo:rustc-env=FUYUBOT_TDLIB_VENDORED_DIR={}",
                dir.display()
            );
            println!("cargo:rustc-env=FUYUBOT_TDLIB_VENDORED_VERSION={version}");
            Some(dir)
        }
        Err(reason) => {
            println!("cargo:warning=vendored TDLib 没能就位：{reason}");
            None
        }
    }
}

// 启用了 feature 却没拿到 cfg，说明构建脚本的 cfg 传播出了问题，明说而不是静默跳过。
#[cfg(not(feature = "vendored"))]
fn acquire_vendored_enabled(_version: &str, _releases_repo: Option<&str>) -> Option<PathBuf> {
    println!("cargo:warning=启用了 vendored feature，但构建脚本没有拿到 feature cfg");
    None
}

/// 把动态库复制到**可执行文件所在目录**（`target/<profile>/`）。
///
/// 为什么必须做：`FUYUBOT_TDLIB_VENDORED_DIR` 烤的是**构建机**的
/// `target/<profile>/build/fuyubot-tdlib-<hash>/out/vendored`，二进制被拷到别的机器或目录之后
/// 那条路不存在，loader 的优先级 2 会**静默失效**（gaps 缺口 3）。放一份到 exe 旁边之后，
/// 优先级 2 的「可执行文件目录」与优先级 4 的裸库名都能命中，
/// 而且 tdjson 的兄弟 DLL 也落在同一个目录里——`open_library` 的
/// `LOAD_WITH_ALTERED_SEARCH_PATH` 靠的正是「库自己所在的目录」（pitfalls 坑 44）。
///
/// 来源按优先级：vendored 目录 → `TDLIB_TDJSON_PATH`（文件或其所在目录）→ 缓存目录
/// `<缓存根>/<版本>/<os>-<arch>`。**一个都找不到就什么都不做**：构建期本来就不依赖 tdjson
/// 存在（约束 7），没装 TDLib 的机器照样能编译。
fn stage_next_to_binary(vendored_dir: Option<&Path>, version: &str) {
    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    // 宁可不拷，也不往一个猜出来的目录里写。
    let Some(dest_dir) = vendored::binary_dir_from(&out_dir) else {
        return;
    };
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let Some(lib_name) = vendored::library_filename(&os) else {
        return;
    };

    for source in staging_sources(vendored_dir, version, &os, &arch) {
        let files = vendored::stageable_library_files(&source, lib_name);
        if files.is_empty() {
            continue;
        }
        for file in files {
            if let Err(error) = copy_library(&file, &dest_dir) {
                println!(
                    "cargo:warning=把 {} 复制到 {} 失败：{error}（打包分发时得自己带上它）",
                    file.display(),
                    dest_dir.display()
                );
            }
        }
        return; // 第一个有货的来源说了算
    }
}

/// 能在哪儿找到动态库，顺序即优先级。
fn staging_sources(
    vendored_dir: Option<&Path>,
    version: &str,
    os: &str,
    arch: &str,
) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();
    if let Some(dir) = vendored_dir {
        sources.push(dir.to_path_buf());
    }
    if let Some(path) = std::env::var_os("TDLIB_TDJSON_PATH").filter(|value| !value.is_empty()) {
        sources.push(PathBuf::from(path));
    }
    for root in build_support::cache_roots() {
        sources.push(root.join(vendored::relative_library_dir(version, os, arch)));
    }
    sources
}

/// 复制（大小一样就当已经拷过——每次构建搬 28MB 没必要）。
fn copy_library(from: &Path, dest_dir: &Path) -> std::io::Result<()> {
    let Some(name) = from.file_name() else {
        return Ok(());
    };
    let dest = dest_dir.join(name);
    let same_size = matches!(
        (from.metadata(), dest.metadata()),
        (Ok(a), Ok(b)) if a.len() == b.len()
    );
    if same_size {
        return Ok(());
    }
    std::fs::create_dir_all(dest_dir)?;
    std::fs::copy(from, &dest).map(|_| ())
}

/// 把当前平台的库拷到 `$OUT_DIR/vendored/`，返回该目录。
#[cfg(feature = "vendored")]
fn acquire(version: &str, releases_repo: Option<&str>) -> Result<PathBuf, String> {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let lib_name = vendored::library_filename(&os)
        .ok_or_else(|| format!("平台 {os}/{arch} 没有约定的动态库文件名"))?;

    let out_dir = std::env::var("OUT_DIR").map_err(|_| "OUT_DIR 未设置".to_owned())?;
    let vendored_dir = PathBuf::from(out_dir).join("vendored");

    let library_asset = vendored::asset_name(&os, &arch).ok_or_else(|| {
        format!(
            "平台 {os}/{arch} 没有对应的 Releases 资产（目前支持：{}）——\
             要加平台得**同时**改 src/loader/vendored.rs 的 ASSETS 表与 \
             .github/workflows/build-tdlib.yml 的 matrix（有守卫测试比对两者）",
            vendored::supported_targets()
        )
    })?;
    let relative = vendored::relative_library(version, &os, &arch);

    // 只有真需要下载时才要求配仓库：缓存命中时不该因为没配 repo 就失败。
    let repo = if build_support::find_cached(&relative).is_some() {
        None
    } else {
        Some(build_support::resolve_repo(releases_repo).ok_or_else(|| {
            format!(
                "没配 Releases 仓库：请在 [package.metadata.fuyubot] 里写 releases-repo，或设置 {}",
                build_support::REPO_ENV
            )
        })?)
    };

    // 动态库：可能是裸库或 tar.gz，交给 extract_library 认。
    // 返回值是**实际落点**——缓存写不进去时它在 $OUT_DIR 下，不能再拿 `relative` 去拷。
    let library_path = fetch_cached(
        repo.as_deref(),
        version,
        library_asset,
        &relative,
        true,
        &os,
    )?;

    std::fs::create_dir_all(&vendored_dir)
        .map_err(|e| format!("创建 {} 失败: {e}", vendored_dir.display()))?;
    let dest = vendored_dir.join(lib_name);
    std::fs::copy(&library_path, &dest).map_err(|e| {
        format!(
            "把 {} 复制到 {} 失败: {e}",
            library_path.display(),
            dest.display()
        )
    })?;
    Ok(vendored_dir)
}

/// 缓存里没有就从 Releases 取回来。`unpack` 为真时按裸库/tar.gz 处理，否则原样存。
///
/// `relative` 是相对**缓存根**的路径（见 [`vendored::relative_library`]）——查找与落盘都用它，
/// 才能跨缓存根保持一致布局。返回值是**库文件实际所在的位置**：缓存写不进去时它在 `$OUT_DIR`
/// 里那份（见 `build_support::store_download`），所以调用方不能假设它等于 `cache_root().join(relative)`。
#[cfg(feature = "vendored")]
fn fetch_cached(
    repo: Option<&str>,
    version: &str,
    asset: &str,
    relative: &Path,
    unpack: bool,
    os: &str,
) -> Result<PathBuf, String> {
    if let Some(hit) = build_support::find_cached(relative) {
        println!("cargo:warning=复用缓存里的 {asset}（{}）", hit.display());
        return Ok(hit);
    }
    let repo = repo.ok_or_else(|| format!("缓存里缺 {asset}，但没有可用的 Releases 仓库"))?;

    let url = build_support::release_url(repo, version, asset);
    println!("cargo:warning=下载 TDLib {version} 的 {asset}：{url}");
    let bytes = build_support::http_get(&url).map_err(|e| format!("下载 {url} 失败: {e}"))?;

    // `.sha256` 侧车文件 404 是正常的（资产可以没有校验文件），拿到什么算什么。
    match build_support::http_get(&format!("{url}.sha256")) {
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
    build_support::store_download(relative, &payload)
}
