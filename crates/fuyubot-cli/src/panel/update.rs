//! 新版本检查：问一次 GitHub 的 Releases API，和本机版本比大小。
//!
//! 边界：**只读一次** GET /repos/<owner>/<repo>/releases/latest——不下载、不解压、不替换
//! 正在运行的 exe（Windows 上那必然失败），面板只把结果说清楚，人自己去 Release 页取。
//! 与商店一样跑在独立线程里，结果经通道回投，事件循环不会被网络吊住。
//!
//! 关掉的办法：[update].enabled = false，或 FUYUBOT_UPDATE_CHECK=0。

use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::Duration;

/// GitHub API 的根：公开仓库不需要 token（匿名限流 60 次/小时，面板一次启动只问一次）。
const API_ROOT: &str = "https://api.github.com";

/// 连不上就别吊着：面板是交互界面，8 秒足够判断「有没有网」。
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Release 的 JSON 很小（几 KB）；512KB 是给超长更新说明留的余量。
const BODY_MAX_BYTES: u64 = 512 * 1024;

/// 本机版本（CLI 的 CARGO_PKG_VERSION，与发布包的版本段同一份）。
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 一次检查的结果：比本机新的版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// Release 的 tag 原文（v0.1.9）。
    pub tag: String,
    /// 归一化后的版本号（0.1.9，去掉 v）。
    pub latest: String,
    /// Release 标题（可能为空，空时界面上只显示 tag）。
    pub name: String,
    /// Release 页面地址（人自己去下载）。
    pub url: String,
    /// 更新说明（只取前几行显示，不解析 Markdown）。
    pub notes: String,
    pub published_at: String,
}

/// 一段版本号的比较口径：只认「主.次.修订」（可带 v 前缀与 -rc.1 这类预发布后缀）。
///
/// 不引入 semver 依赖：宿主版本号就是三段数字，预发布只用来「不把 rc 当成正式版」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 带连字符后缀（0.1.9-rc.1）= 预发布：同号下比正式版**小**。
    pub pre_release: bool,
}

impl Version {
    /// 解析；容忍开头的 v / V，主干后多余的段（0.1.9.4）忽略。
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        if text.is_empty() {
            return None;
        }
        // 预发布 / 构建元数据都从第一个连字符或加号处切开，只看主干。
        let (core, tail) = match text.find(['-', '+']) {
            Some(position) => (&text[..position], &text[position..]),
            None => (text, ""),
        };
        let pre_release = tail.starts_with('-');
        let mut parts = core.split('.');
        let major = parse_number(parts.next()?)?;
        let minor = parse_number(parts.next().unwrap_or("0"))?;
        let patch = parse_number(parts.next().unwrap_or("0"))?;
        Some(Self {
            major,
            minor,
            patch,
            pre_release,
        })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            // 同号时：正式版 > 预发布版（0.1.9 > 0.1.9-rc.1）。
            .then_with(|| other.pre_release.cmp(&self.pre_release))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_number(text: &str) -> Option<u64> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// latest 比 current 新吗；任一侧解析不出来就是「不知道」（None）。
pub fn is_newer(current: &str, latest: &str) -> Option<bool> {
    let current = Version::parse(current)?;
    let latest = Version::parse(latest)?;
    Some(latest > current)
}

/// 仓库名必须是 owner/name：它会被拼进 URL，别的形状一律拒绝。
pub fn check_repo(repo: &str) -> Result<String, String> {
    let repo = repo.trim();
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!(
            "仓库名 {repo:?} 不是 owner/repo 的形状（例：CatMoeCircle/Fuyu-Bot）"
        ));
    };
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
    };
    if !valid(owner) || !valid(name) {
        return Err(format!(
            "仓库名 {repo:?} 只允许 [A-Za-z0-9._-]：检查 [update].repo / FUYUBOT_UPDATE_REPO"
        ));
    }
    Ok(format!("{owner}/{name}"))
}

/// 解析 releases/latest 的响应体；比本机新才返回 Some。
///
/// 纯函数：测试直接喂 JSON，不联网。
pub fn parse_latest(bytes: &[u8], current: &str) -> Result<Option<UpdateInfo>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("GitHub 的返回不是 JSON：{error}（API 改版或代理插了内容）"))?;
    let draft = value
        .get("draft")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let prerelease = value
        .get("prerelease")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if draft || prerelease {
        return Ok(None);
    }
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| "GitHub 的返回里没有 tag_name：这个 Release 不完整".to_owned())?;
    let latest = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
        .to_owned();
    match is_newer(current, &latest) {
        Some(true) => {}
        Some(false) => return Ok(None),
        None => {
            return Err(format!(
                "版本号对不上：GitHub 上是 {tag:?}，本机是 {current:?}（tag 要写成 v主.次.修订）"
            ));
        }
    }
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    Ok(Some(UpdateInfo {
        tag: tag.to_owned(),
        latest,
        name: text("name"),
        url: text("html_url"),
        notes: text("body"),
        published_at: text("published_at"),
    }))
}

/// 问一次 GitHub：Ok(None) = 已是最新，Err = 没查成（原文给人看）。
pub fn check(repo: &str, current: &str) -> Result<Option<UpdateInfo>, String> {
    let repo = check_repo(repo)?;
    let url = format!("{API_ROOT}/repos/{repo}/releases/latest");
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_resolve(Some(HTTP_TIMEOUT))
            .timeout_connect(Some(HTTP_TIMEOUT))
            .timeout_recv_response(Some(HTTP_TIMEOUT))
            .timeout_recv_body(Some(HTTP_TIMEOUT))
            .build(),
    );
    // GitHub 的 API 强制要求 User-Agent（不带会被 403 挡回来）。
    let request = agent
        .get(&url)
        .header("User-Agent", format!("fuyubot/{}", current_version()))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    let mut response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => {
            return Err(format!("{repo} 还没有发布过 Release（或仓库名写错）：404"));
        }
        Err(ureq::Error::StatusCode(code @ (403 | 429))) => {
            return Err(format!(
                "GitHub API 限流了（HTTP {code}）：等一会儿再按 U，或设 FUYUBOT_UPDATE_CHECK=0 关掉检查"
            ));
        }
        Err(error) => return Err(format!("连不上 GitHub：{error}（检查网络 / 代理）")),
    };
    let bytes = response
        .body_mut()
        .with_config()
        .limit(BODY_MAX_BYTES)
        .read_to_vec()
        .map_err(|error| format!("读 GitHub 的返回失败：{error}"))?;
    parse_latest(&bytes, current)
}

/// 在后台线程里查一次：结果经通道回投给面板（事件循环只 try_recv，绝不阻塞）。
pub fn spawn_check(repo: String, current: String) -> Receiver<Result<Option<UpdateInfo>, String>> {
    let (tx, rx) = channel();
    let fallback = tx.clone();
    let spawned = thread::Builder::new()
        .name("fuyubot-update-check".to_owned())
        .spawn(move || {
            let result = check(&repo, &current);
            let _ = tx.send(result);
        });
    if let Err(error) = spawned {
        let _ = fallback.send(Err(format!("起不了检查线程：{error}")));
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_an_optional_v_prefix() {
        assert_eq!(
            Version::parse("v0.1.9"),
            Some(Version {
                major: 0,
                minor: 1,
                patch: 9,
                pre_release: false
            })
        );
        assert_eq!(Version::parse("1.2.3").map(|v| v.patch), Some(3));
        // 只写两段按修订 0 算（v1.2 这种 tag 也认）。
        assert_eq!(Version::parse("1.2").map(|v| v.patch), Some(0));
        assert!(Version::parse("not-a-version").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn a_newer_patch_wins_and_a_prerelease_loses() {
        assert_eq!(is_newer("0.1.8", "0.1.9"), Some(true));
        assert_eq!(is_newer("0.1.9", "0.1.9"), Some(false));
        assert_eq!(is_newer("0.2.0", "0.1.9"), Some(false));
        // 同号里正式版比 rc 新；rc 不比正式版新。
        assert_eq!(is_newer("0.1.9-rc.1", "0.1.9"), Some(true));
        assert_eq!(is_newer("0.1.9", "0.1.9-rc.2"), Some(false));
    }

    #[test]
    fn the_release_json_is_read_and_only_a_newer_tag_is_reported() {
        let json = "{
            \"tag_name\": \"v0.1.9\",
            \"name\": \"fuyubot v0.1.9\",
            \"html_url\": \"https://github.com/CatMoeCircle/Fuyu-Bot/releases/tag/v0.1.9\",
            \"body\": \"- 新版本检查\\n- 按 id 安装\",
            \"published_at\": \"2026-11-01T00:00:00Z\",
            \"prerelease\": false,
            \"draft\": false
        }";
        let info = parse_latest(json.as_bytes(), "0.1.8")
            .expect("该解析成功")
            .expect("该有新版本");
        assert_eq!(info.latest, "0.1.9");
        assert_eq!(info.tag, "v0.1.9");
        assert!(info.url.contains("releases/tag/v0.1.9"));
        assert!(info.notes.contains("按 id 安装"));
        // 已经是最新：不报。
        assert!(parse_latest(json.as_bytes(), "0.1.9").unwrap().is_none());
        // 预发布 / 草稿不提示。
        let pre = br#"{"tag_name":"v0.2.0","prerelease":true,"draft":false}"#;
        assert!(parse_latest(pre, "0.1.8").unwrap().is_none());
        let draft = br#"{"tag_name":"v0.2.0","prerelease":false,"draft":true}"#;
        assert!(parse_latest(draft, "0.1.8").unwrap().is_none());
        // 坏 JSON / 缺 tag 都要报出来，而不是静默当「已是最新」。
        assert!(parse_latest(b"not json", "0.1.8").is_err());
        assert!(parse_latest(br#"{"name":"x"}"#, "0.1.8").is_err());
        // tag 口径不对（没法比大小）同样报错。
        assert!(parse_latest(br#"{"tag_name":"latest"}"#, "0.1.8").is_err());
    }

    #[test]
    fn only_owner_slash_repo_is_accepted() {
        assert_eq!(
            check_repo(" CatMoeCircle/Fuyu-Bot ").unwrap(),
            "CatMoeCircle/Fuyu-Bot"
        );
        for bad in [
            "",
            "owner",
            "owner/",
            "/repo",
            "a/b/c",
            "own er/repo",
            "owner/re po",
        ] {
            assert!(check_repo(bad).is_err(), "{bad:?} 该被拒绝");
        }
    }
}
