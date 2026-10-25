//! `td_api.tl` 在**编译期**的获取与转换。
//!
//! `td_api.tl` 是**必需输入**：无条件去拿（来源是 `TDLIB_TD_API_TL` → 缓存 → Releases 下载），
//! 产出两份文件——`$OUT_DIR/td_api.json`（运行时的元数据）与
//! `$OUT_DIR/td_api_generated.rs`（Rust 类型），运行时分别用 `include_str!` / `include!` 烤进二进制。
//! **转换只在编译期发生**，运行时不读文件、不解析、不生成。
//!
//! 任何读取/解析失败都是**构建失败**（见 `bake_schema`）：转换是必需的一步，不是尽力而为。
//!
//! vendored **原生库**的获取不在这里——那是 `fuyubot-tdlib/build.rs` 的事；
//! 两个脚本共享的缓存/下载设施在 `src/build_support.rs`（见那份文件的模块文档）。

// 共享的缓存与下载设施（缓存根、http_get、read_metadata、release_url…）。
#[allow(dead_code)] // 未被本脚本用到的函数（如动态库那半）仍然要完整编译
#[path = "src/build_support.rs"]
mod build_support;

// 与 src/schema.rs 共用同一份解析与 JSON 互转：编译期写盘、运行时读回，形状只能是同一个实现。
#[allow(dead_code)]
#[path = "src/tl_schema.rs"]
mod tl_schema;

// 从 td_api.tl 生成 Rust 类型（`src/td_api.rs` 用 include! 展出来）。
#[allow(dead_code)]
#[path = "src/tl_codegen.rs"]
mod tl_codegen;

// tl_codegen 生成文档时经 `crate::docs` 转 rustdoc——构建脚本的编译单元里也得有它。
#[allow(dead_code)]
#[path = "src/docs.rs"]
mod docs;

use std::path::{Path, PathBuf};

/// `releases-repo` 没填真实值时用的占位符：下载必然 404，所以出错时要专门点出来。
const PLACEHOLDER_REPO: &str = "fuyubot/fuyubot";

fn main() {
    for key in [
        "TDLIB_TD_API_TL",
        build_support::CACHE_ENV,
        build_support::REPO_ENV,
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_support.rs");
    // build.rs 与运行时共用这三个文件，它们变了要重新转换。
    println!("cargo:rerun-if-changed=src/tl_schema.rs");
    println!("cargo:rerun-if-changed=src/tl_codegen.rs");
    println!("cargo:rerun-if-changed=src/docs.rs");

    let fuyubot = build_support::read_metadata().unwrap_or_default();
    let version = fuyubot.tdlib_version.unwrap_or_else(|| "0.0.0".to_owned());

    // td_api.tl 是**必需**输入，拿不到就让构建失败（见 acquire_schema）。
    let schema = acquire_schema(&version, fuyubot.releases_repo.as_deref());
    bake_schema(&schema);
}

/// **无条件**把 `td_api.tl` 弄到手——它是构建的必需输入，不属于 vendored 那一套。
///
/// 顺序：`TDLIB_TD_API_TL`（显式指定）→ 缓存（见 [`find_cached`]：默认缓存根 → 项目本地）
/// → 从 Releases 下载。下载成功会写进缓存，所以一台机器只会真正下载一次。
///
/// **拿不到就让构建失败**：不存在「没有就不转换」这条路。
/// 但「**缓存写不进去**」不是拿不到——见 [`store_download`]，那种情况会退到项目本地缓存。
fn acquire_schema(version: &str, configured_repo: Option<&str>) -> PathBuf {
    if let Some(path) = explicit_schema() {
        return path;
    }

    // 缓存命中就**静默**复用：用的是哪份、多少条，`bake_schema` 那条汇总日志已经带了路径，
    // 再说一遍只是每次重跑构建脚本都刷一条 warning（用户 2026 抱怨过）。
    let relative = Path::new(version).join("td_api.tl");
    if let Some(cached) = build_support::find_cached(&relative) {
        return cached;
    }

    let Some(repo) = build_support::resolve_repo(configured_repo) else {
        panic!("{}", missing_repo_hint());
    };
    let url = build_support::release_url(&repo, version, "td_api.tl");
    println!("cargo:warning=下载 TDLib {version} 的 td_api.tl：{url}");
    let bytes = build_support::http_get(&url)
        .unwrap_or_else(|error| panic!("{}", download_hint(&url, &error, &repo)));

    let stored =
        build_support::store_download(&relative, &bytes).unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:warning=已缓存到 {}", stored.display());
    stored
}

/// `TDLIB_TD_API_TL` 指定的那份（文件或目录）。
///
/// 它优先级最高：是离线环境、以及「用自己那份 schema」的出口。
/// **相对路径会按「as-given → crate 根 → workspace 根」依次试**：build.rs 的 CWD 是
/// crate 根目录，而人多半是在 workspace 根敲命令，两边都得认。
fn explicit_schema() -> Option<PathBuf> {
    let given = std::env::var_os("TDLIB_TD_API_TL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    let given = if given.is_dir() {
        given.join("td_api.tl")
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
        "TDLIB_TD_API_TL 指向的 {} 不存在（拿不到 td_api.tl 就没法生成 TDLib 类型）。\n试过：{tried:?}",
        given.display()
    );
}

fn missing_repo_hint() -> String {
    format!(
        "没配 Releases 仓库，拿不到 td_api.tl。\n\
         它是构建的必需输入（要把 TDLib API 转成 Rust 类型），三个办法：\n\
         ① 在 crates/fuyubot-tdlib-sys/Cargo.toml 的 [package.metadata.fuyubot] 里写真实的 releases-repo；\n\
         ② 设 {} 覆盖它；\n\
         ③ 设 TDLIB_TD_API_TL 指向本地那份 td_api.tl。",
        build_support::REPO_ENV
    )
}

fn download_hint(url: &str, error: &str, repo: &str) -> String {
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
         拿不到 td_api.tl 就没法生成 TDLib 类型，而它是构建的必需输入。\n\
         ① 确认 {repo} 的 release 里有这个资产，标签形如 `TDLib-v1.8.66`；\n\
         ② 或者设 TDLIB_TD_API_TL 指向本地那份；\n\
         ③ 或者手动把它放到 <缓存根>/<版本>/td_api.tl（缓存根见上面那条 warning）。",
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
