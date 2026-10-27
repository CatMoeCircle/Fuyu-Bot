//! 构建脚本共享的缓存与下载设施。
//!
//! 这个文件被 **两处** `#[path]` 引入，外加一处测试编译：
//! - `crates/fuyubot-tdlib-sys/build.rs` —— 拿 `td_api.tl`（本 crate 自己，路径 `src/…`）；
//! - `crates/fuyubot-tdlib/build.rs` —— 拿 vendored 动态库（跨 crate 路径）；
//! - 本 crate 的 `lib.rs` 在 `#[cfg(test)]` 下引入 —— 为了跑文件自带的测试。
//!
//! 所以它只能依赖 std + 构建脚本都有的那几个构建依赖（`ureq` / `serde` / `toml`），
//! **不得引用任何 crate 内其他模块**，也不得进正常编译的依赖图（ureq 不该出现在运行时）。
//!
//! 之所以共享：schema 与动态库**用同一串缓存根**（`FUYUBOT_TDLIB_VENDORED_CACHE` →
//! `$CARGO_HOME/…` → 项目本地 `.tdlib-cache`）。两套实现各自演化的话，
//! 「查找」和「落盘」会落进不同的根——表现为「明明缓存了，下次构建还是重下」。

use std::path::{Path, PathBuf};

/// 缓存根目录环境变量。
pub const CACHE_ENV: &str = "FUYUBOT_TDLIB_VENDORED_CACHE";
/// 覆盖 `[package.metadata.fuyubot] releases-repo` 的环境变量。
pub const REPO_ENV: &str = "FUYUBOT_TDLIB_RELEASES_REPO";

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

/// **所有**缓存根，顺序即优先级：
/// ① [`cache_root`]（`FUYUBOT_TDLIB_VENDORED_CACHE` → `$CARGO_HOME/fuyubot-tdlib-vendored`）；
/// ② **项目本地** `<工作区根>/.tdlib-cache`（见 [`workspace_local_cache`]）。
///
/// 为什么要有 ②：`$CARGO_HOME` 常常写不进去（只读 `$HOME` 的容器、受限 CI、DSH 的
/// workspace-write 沙箱），那时只能退到 `$OUT_DIR`——而 **cargo 每重跑一次构建脚本都会清空
/// `$OUT_DIR`**，于是「缓存写不进去」的环境里**每次构建都重新下载**。
/// 项目本地这份在工作区内、`target/` 之外：受限环境里可写，而且**跨构建复用**。
///
/// ⚠️ **查找与落盘必须用同一份列表**：只写进 ② 却只查 ① 的话，下次构建照样重新下载。
pub fn cache_roots() -> Vec<PathBuf> {
    let mut roots = vec![cache_root()];
    if let Some(local) = project_cache_root()
        && !roots.contains(&local)
    {
        roots.push(local);
    }
    roots
}

/// 构建期的项目本地缓存：从 `CARGO_MANIFEST_DIR` 往上找带 `[workspace]` 的 `Cargo.toml`。
///
/// 运行期没有 `CARGO_MANIFEST_DIR`，所以这里返回 `None`——运行时找 vendored 目录靠的是
/// `fuyubot-tdlib` 编译期烤进去的 `FUYUBOT_TDLIB_VENDORED_DIR`，不走这条路。
fn project_cache_root() -> Option<PathBuf> {
    workspace_local_cache(Path::new(&std::env::var_os("CARGO_MANIFEST_DIR")?))
}

/// `start` 所属工作区的项目本地缓存目录；**不是真工作区就返回 `None`**。
///
/// 判据是「往上某一级的 `Cargo.toml` 里真有 `[workspace]`」：本仓是
/// `<根>/crates/<crate>`（上两级），单 crate 项目就是 `<根>` 本身，都认；
/// 而依赖方的 **registry 源码目录**往上也有一堆 `Cargo.toml`，但那些都不是工作区根——
/// 往里写 `.tdlib-cache` 毫无意义，还会污染用户的 cargo 缓存目录。
pub fn workspace_local_cache(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if is_workspace_root(current) {
            return Some(current.join(".tdlib-cache"));
        }
        dir = current.parent();
    }
    None
}

/// 这个目录的 `Cargo.toml` 是不是工作区根（有 `[workspace]` 段）。
fn is_workspace_root(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .is_ok_and(|text| text.lines().any(|line| line.trim() == "[workspace]"))
}

/// Releases 下载 URL。Tag 使用无空格的 `TDLib-v{version}`，避免 Git tag 非法。
pub fn release_url(repo: &str, version: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/TDLib-v{version}/{asset}")
}

/// `[package.metadata.fuyubot]`（读**当前构建脚本所在 crate** 的 `Cargo.toml`；读不到就当没配）。
///
/// 两个 crate 各有一份同名元数据（schema 与动态库都要版本和仓库），
/// 值必须一致——守卫测试 [`metadata_is_in_sync_between_the_two_crates`] 比对两边。
pub fn read_metadata() -> Option<Fuyubot> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let text = std::fs::read_to_string(Path::new(&manifest).join("Cargo.toml")).ok()?;
    let parsed: Manifest = toml::from_str(&text).ok()?;
    parsed.package.metadata?.fuyubot
}

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

/// `[package.metadata.fuyubot]` 里两个构建脚本都要用的字段
/// （版本：schema 下载 + 动态库缓存路径 + 版本协商；仓库：两边各自的下载）。
#[derive(serde::Deserialize, Default)]
pub struct Fuyubot {
    #[serde(rename = "tdlib-version")]
    pub tdlib_version: Option<String>,
    #[serde(rename = "releases-repo")]
    pub releases_repo: Option<String>,
}

/// Releases 仓库：环境变量优先，其次 `[package.metadata.fuyubot] releases-repo`。
pub fn resolve_repo(configured: Option<&str>) -> Option<String> {
    std::env::var(REPO_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            configured
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

/// 在所有缓存根里找 `relative` 那一份（顺序即优先级，见 [`cache_roots`]）。
pub fn find_cached(relative: &Path) -> Option<PathBuf> {
    cache_roots()
        .into_iter()
        .map(|root| root.join(relative))
        .find(|path| path.is_file())
}

/// 把下载到的字节落盘：**先按缓存根顺序试**，全写不进去才退 [`store_out_dir_fallback`]。
///
/// ⚠️ **`$OUT_DIR` 不是缓存**：cargo 每次重跑构建脚本都会清掉它，所以「退到 OUT_DIR」=
/// 下一个构建**重新下载**（`~/.cargo` 在 DSH 沙箱里只读 →
/// 每次构建重下一遍，每次都刷三条 warning）。
/// 所以顺序是「**默认缓存根** → **项目本地 `.tdlib-cache`**（跨构建复用）→ OUT_DIR（保底，会重下）」。
///
/// 返回**实际落盘的位置**——调用方必须用它，不能再用请求的那个路径（两者可能不同）。
pub fn store_download(relative: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let roots = cache_roots();
    let mut blocked: Option<(PathBuf, std::io::Error)> = None;
    for root in &roots {
        let dest = root.join(relative);
        // 目录要自己建（`<root>/<version>/…` 通常还不存在）；建不出来 = 这个根写不进去。
        let outcome = match dest.parent() {
            Some(dir) => std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&dest, bytes)),
            None => std::fs::write(&dest, bytes),
        };
        match outcome {
            Ok(()) => {
                // 第一顺位就成功 = 一切照常，不用解释；换了根才要说清「为什么不是默认那个」。
                if let Some((blocked, error)) = blocked.as_ref() {
                    println!(
                        "cargo:warning={} 写不进去（{error}）；这次改写到 {}。\
                         项目本地缓存跨构建复用，不会再重复下载",
                        blocked.display(),
                        dest.display()
                    );
                }
                return Ok(dest);
            }
            Err(error) => {
                if blocked.is_none() {
                    blocked = Some((dest, error));
                }
            }
        }
    }

    let (blocked, error) = blocked.expect("cache_roots 至少有一项");
    println!("cargo:warning=缓存根都写不进去（{error}）");
    store_out_dir_fallback(&blocked, bytes)
}

/// 最后的保底：写进 `$OUT_DIR/tdlib-download/`（**下次构建会被清掉，于是重新下载**）。
///
/// ⚠️ 这里以前是 `create_dir_all(...).unwrap_or_else(panic!)`，于是 `$CARGO_HOME` 不可写时
/// 构建**直接失败**——而失败信息是「创建 `~/.cargo/fuyubot-tdlib-vendored/1.8.66` 失败：拒绝访问
/// (os error 5)」，明明下载已经成功，却完全指向不了原因。踩过的场景：受限 CI、只读 `$HOME` 的容器、
/// 以及 DSH 的 workspace-write 沙箱（写仓库外一律拒）。见 pitfalls 坑 80。
fn store_out_dir_fallback(dest: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    fn write_at(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, bytes)
    }

    match write_at(dest, bytes) {
        Ok(()) => Ok(dest.to_path_buf()),
        Err(cache_error) => {
            let out_dir = std::env::var("OUT_DIR").map_err(|_| {
                format!(
                    "写 {} 失败：{cache_error}；而且 OUT_DIR 未设置",
                    dest.display()
                )
            })?;
            let name = dest
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("tdlib-download"));
            let fallback = PathBuf::from(out_dir).join("tdlib-download").join(name);
            write_at(&fallback, bytes).map_err(|e| {
                format!(
                    "缓存 {} 写不进去（{cache_error}），退到 {} 也写不进去（{e}）\n\
                     试过：设环境变量 {} 指向一个可写的缓存目录",
                    dest.display(),
                    fallback.display(),
                    CACHE_ENV
                )
            })?;
            println!(
                "cargo:warning=缓存 {} 写不进去（{cache_error}）；本次改用 {}。\
                 缓存只是优化，构建继续——想固定缓存位置就设 {}",
                dest.display(),
                fallback.display(),
                CACHE_ENV
            );
            Ok(fallback)
        }
    }
}

/// 构建期的 HTTP GET。
///
/// **不按 `vendored` 门控**：`td_api.tl` 是必需输入，无论开不开 vendored 都要下载它。
/// `ureq` 因此是常开的 build-dependency（只在构建期，不进运行时的依赖图）。
///
/// `#[cfg(not(test))]`：本文件在 `cfg(test)` 下编译进来只是为了跑缓存布局那几个测试，
/// 而 `http_get` 一个测试都没有——把 `ureq` 拖进测试编译只会把它的传递依赖
/// （ring 的 C 代码）也拉进来，纯属浪费。
///
/// 两条刻意的取舍：
/// - **瞬时失败重试**：`td_api.tl` 下不来 = 构建失败，一次网络抖动不该毁掉整次构建。
/// - **不设超时**：ureq 的默认全是 `None`（即不超时），这正是慢速链路下拖 12MB 动态库资产
///   所需要的；给它加超时会把「慢但能成」变成失败。也别为了「可控」随手 `timeout_global`。
/// - **4xx 不重试**：仓库名 / tag / 资产名写错是确定性的，重试只是浪费时间。
#[cfg(not(test))]
pub fn http_get(url: &str) -> Result<Vec<u8>, String> {
    const ATTEMPTS: u32 = 3;
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match ureq::get(url).call() {
            Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => {
                return Err(format!("HTTP {code}"));
            }
            Ok(response) => match response.into_body().read_to_vec() {
                Ok(bytes) => return Ok(bytes),
                Err(error) => last = error.to_string(),
            },
            Err(error) => last = error.to_string(),
        }
        if attempt < ATTEMPTS {
            println!("cargo:warning=下载 {url} 失败（{last}），重试（{attempt}/{ATTEMPTS}）");
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的临时目录（**不用 `%TEMP%`**，见 AGENTS 的测试规矩）。
    fn temp(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-tdlib-sys")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }

    /// 项目本地缓存只在**真工作区**里才算数。
    ///
    /// 这条为什么值得有自检：判据写宽了（比如「往上随便哪个 `Cargo.toml`」）就会在
    /// **依赖方的 registry 源码目录**里找一个 `.tdlib-cache`——那既没用，又往用户
    /// 不该碰的地方落文件；写窄了（只认「上两级」）又认不出别的布局。
    #[test]
    fn the_project_local_cache_is_only_used_inside_a_real_workspace() {
        // 工作区根 + 它的成员 crate（本仓的布局：crate 在根下面两层）。
        let root = temp("workspace-cache");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let member = root.join("crates").join("fuyubot-tdlib-sys");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(
            workspace_local_cache(&member),
            Some(root.join(".tdlib-cache"))
        );
        // 根自己也算（单 crate 项目就是这种形状）。
        assert_eq!(
            workspace_local_cache(&root),
            Some(root.join(".tdlib-cache"))
        );

        // 只有 `[package]` 的目录不是工作区根。
        let plain = temp("plain-crate");
        std::fs::write(plain.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(!is_workspace_root(&plain), "[package] 不算工作区根");
        assert!(is_workspace_root(&root));
        // 连 Cargo.toml 都没有的目录也不能算（读不出来就是 false）。
        assert!(!is_workspace_root(&plain.join("src")));
    }

    #[test]
    fn release_url_encodes_the_space_in_the_tag() {
        assert_eq!(
            release_url("fuyubot/fuyubot", "1.8.66", "TDLib-windows-x86_64"),
            "https://github.com/fuyubot/fuyubot/releases/download/TDLib-v1.8.66/TDLib-windows-x86_64"
        );
    }

    /// 两份 `[package.metadata.fuyubot]` 必须一致（schema 与动态库各读各的 Cargo.toml）。
    ///
    /// 漂了的表现是「schema 是 A 版本转的，动态库是 B 版本」——两份 `td_api.tl`/tdjson
    /// 对不上时错误会出现在完全无关的地方，所以钉在这里。
    #[test]
    fn metadata_is_in_sync_between_the_two_crates() {
        let mine: Fuyubot = toml::from_str(
            &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
                .unwrap(),
        )
        .ok()
        .and_then(|parsed: Manifest| parsed.package.metadata?.fuyubot)
        .expect("本 crate 的 [package.metadata.fuyubot] 读不出来");

        let sibling = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("fuyubot-tdlib")
            .join("Cargo.toml");
        let theirs: Fuyubot = toml::from_str(&std::fs::read_to_string(&sibling).unwrap())
            .ok()
            .and_then(|parsed: Manifest| parsed.package.metadata?.fuyubot)
            .expect("fuyubot-tdlib 的 [package.metadata.fuyubot] 读不出来");

        assert_eq!(
            mine.tdlib_version, theirs.tdlib_version,
            "tdlib-version 两处不一致——schema 与动态库会来自不同版本"
        );
        assert_eq!(
            mine.releases_repo, theirs.releases_repo,
            "releases-repo 两处不一致——两边会去不同的仓库下载"
        );
        assert!(
            mine.tdlib_version.is_some(),
            "本 crate 忘了写 tdlib-version"
        );
    }
}
