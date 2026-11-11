//! 运行时配置（`fuyubot.toml`）：CLI 与 Android JNI **共用同一份** schema、模板与解析。
//!
//! 为什么放这里而不是留在 CLI：两个宿主读的是同一个文件（Android 也从 `<root>/fuyubot.toml`
//! 起，见 `fuyubot-android` 的 `Options`）。schema 留在一个宿主里、另一个再抄一份，迟早漂移；
//! 模板也只允许有一份（约束 39），所以它跟着 schema 一起搬到这里。
//!
//! 与凭据的边界（约束 41）：`api_hash` **不进 [`Config`]**——`Config::to_json()` 的整棵树会交给
//! 宿主运行时，而插件经 `get_config` 能读**任意**段。要读 hash 请用 [`load_api_hash`]，
//! 写回请用 [`remember_credentials`]（都是文本级操作，保留注释）。

use std::path::{Path, PathBuf};

use fuyubot_loader::{DEFAULT_CACHE_CAPACITY, DEFAULT_LOG_VERBOSITY, LogLevel};

/// 项目配置文件名。
pub const CONFIG_FILE: &str = "fuyubot.toml";

/// 配置模板（`include_str!`）：`fuyubot init` 与 Android 的 `config_init` 渲染的是同一份。
pub const CONFIG_TEMPLATE: &str = include_str!("../templates/fuyubot.toml");

/// `Config.plugin` 的默认值：一张空表（没有 `[plugin.*]` 段时）。
fn empty_table() -> toml::Value {
    toml::Value::Table(toml::Table::new())
}

// ── 项目配置 ───────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Config {
    #[serde(default)]
    pub bot: Bot,
    #[serde(default)]
    pub telegram: Telegram,
    /// 命令头数组与触发者白名单（`[command]`）。不写命令头时 bot 认 `/`、user 认 `.`。
    #[serde(default)]
    pub command: Command,
    /// 主人 / 全局管理员（`[roles]`）。跨命令的能力，和「谁能触发」的白名单分开。
    #[serde(default)]
    pub roles: Roles,
    /// TDLib 对象缓存（`[cache]`）。
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub plugins: Plugins,
    #[serde(default)]
    pub tdlib: Tdlib,
    /// 每个插件自己的配置段：`[plugin.<插件名>]`。插件名是点分的（如 `fuyubot.manage`），
    /// 在 TOML 里自然写成嵌套表 `[plugin.fuyubot.manage]`——所以这里存整棵 `plugin` 表
    /// （`toml::Value`），宿主把插件名当**点路径**在它里面下钻。
    /// 插件经 ABI 的 `get_config` 读自己/别人的段、经 `set_config` 写自己的段。
    #[serde(default = "empty_table")]
    pub plugin: toml::Value,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bot: Bot::default(),
            telegram: Telegram::default(),
            command: Command::default(),
            roles: Roles::default(),
            cache: CacheConfig::default(),
            plugins: Plugins::default(),
            tdlib: Tdlib::default(),
            plugin: empty_table(),
        }
    }
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Bot {
    pub name: Option<String>,
    /// TDLib api_id 的**旧位置**（[`Telegram::api_id`] 优先）。新配置写进 `[telegram]`。
    ///
    /// `api_hash` 这里**故意没有**字段：它是凭据，只允许从环境变量 `TG_API_HASH`
    /// 或交互式输入来（硬约束 41）。写了也会被 serde 忽略。
    pub api_id: Option<i64>,
}

/// `[telegram]`：TDLib 参数与**账号类型**。
///
/// ⚠️ 凭据（`api_hash` / token / 手机号）**不在这里**（硬约束 41）——只落**类型**。
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Telegram {
    /// `"bot"`（默认）或 `"user"`——决定登录走哪条路（Bot Token / 手机号 / 扫码）。
    pub account_type: Option<String>,
    /// TDLib 的 `database_directory` 与 `files_directory` **都**指它（decisions 变更 1：
    /// 只建 `data/`，不拆 `data/db`）。相对项目根目录。
    pub data_dir: Option<String>,
    /// TDLib api_id（应用凭据，不是秘密）。两处都没写就用 0。
    pub api_id: Option<i64>,
}

impl Telegram {
    /// 认账号类型。**写错就报错指路**，不静默当 bot——否则用户会拿着错的登录方式
    /// 一直等不到验证码，而错误信息完全不指向配置。
    ///
    /// 大小写与首尾空格都容忍（`" User "` 也认）：手写配置里这两种很常见，
    /// 而 `Bot` / `User` 本来就无歧义——trim 了却不忽略大小写是自相矛盾的。
    pub fn account_type(&self) -> Result<fuyubot_tdlib::AccountType, String> {
        let raw = self
            .account_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("bot");
        match raw.to_ascii_lowercase().as_str() {
            "bot" => Ok(fuyubot_tdlib::AccountType::Bot),
            "user" => Ok(fuyubot_tdlib::AccountType::User),
            _ => Err(format!(
                "[telegram].account_type = \"{raw}\" 认不出来，只能写 \"bot\" 或 \"user\""
            )),
        }
    }

    /// `[telegram].api_id` 优先，退回旧的 `[bot].api_id`。
    pub fn api_id(&self, bot: &Bot) -> i64 {
        self.api_id.or(bot.api_id).unwrap_or(0)
    }

    /// TDLib 的数据目录（配置里没写就 `<root>/data`）。
    pub fn data_dir(&self, root: &Path) -> PathBuf {
        root.join(self.data_dir.as_deref().unwrap_or("data"))
    }
}

/// `[command]`：命令头数组、触发者白名单与白名单模式开关。
///
/// 真正的判定在宿主（`fuyubot-loader`）：没配命令头时 bot 认 `/`、user 认 `.`；
/// user 账户下 `/xxx` 一律不交给插件（即使写进数组），其它命令头放行后归一化成 `/xxx`；
/// `whitelist` 里的用户 id 也能触发。user 账户默认**白名单模式**（只认自己 + 表里的
/// 用户），`whitelist_only = false` 可关，但**不建议**。
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Command {
    /// 命令头数组，例如 `["/", ".", "。"]`。没写 = 按账号类型取默认值
    /// （bot `/`、user `.`）；每项必须是单个字符（不能是字母、数字或空白）。
    pub prefix: Option<Vec<String>>,
    /// 允许触发命令的用户 id（数字）。user 账户下自己始终算数；空表时
    /// user 只认自己、bot 不限制。
    #[serde(default)]
    pub whitelist: Vec<i64>,
    /// user 账户的白名单模式（默认 `true`）：只响应自己与 `whitelist` 里的用户。
    ///
    /// `Some(false)` = 关闭，任何人发的命令都能触发——**不建议**，模板与文档的注释
    /// 必须写明这一点。bot 账户不受它影响。
    pub whitelist_only: Option<bool>,
}

impl Command {
    /// 配置里写的命令头；没写 / 空数组返回 `None`（宿主按账号类型取默认值）。
    pub fn prefixes(&self) -> Option<&[String]> {
        self.prefix
            .as_deref()
            .filter(|prefixes| !prefixes.is_empty())
    }

    /// user 账户的白名单模式；没写 = `true`。
    pub fn whitelist_only(&self) -> bool {
        self.whitelist_only.unwrap_or(true)
    }
}

/// `[roles]`：主人与全局管理员。
///
/// 它们决定「触发之后能做什么」，不是「能不能触发」——后者仍是 `[command].whitelist`。
/// 群里的 Telegram 管理员是**会话角色**，由宿主用 `getChatMember` 现查，不在这张表里。
#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Roles {
    /// 主人：最高权限。bot 账户没有隐式主人，必须显式配置；user 账户在列表为空时
    /// 默认自己是主人，配置了列表则按列表来（可以换成别人）。
    #[serde(default)]
    pub owners: Vec<i64>,
    /// 全局管理员：能用 `admin` 级命令，但不能用 `owner` 级。
    #[serde(default)]
    pub admins: Vec<i64>,
}

/// `[cache]`：宿主 TDLib 对象缓存。
///
/// 全量存 TDLib 原始 JSON，每种类型默认 100 条。100 条全量 `Value` 大约几 MiB；
/// `capacity` 是**每种类型**的上限，不是总数。
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct CacheConfig {
    /// 默认 `true`。关掉后 `Simplify` 不再读写缓存，但连接状态判断仍然保留。
    pub enabled: Option<bool>,
    /// 每种类型保留多少条（`chat` / `user` / `basic_group` / `supergroup` /
    /// `chat_member` / `community` / `community_full_info` 各一份配额）。
    pub capacity: Option<usize>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            capacity: None,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Plugins {
    pub dir: Option<String>,
    /// 插件日志的最低级别：`error` / `warn` / `info`（默认）/ `debug` / `trace`。
    ///
    /// 它只决定宿主**记不记**：低于门槛的既不留档也不写 stderr。插件照旧能调 `log`。
    pub log_level: Option<String>,
    /// TUI 插件商店的索引地址（静态 JSON）。没配 = 商店页显示「未配置」。
    ///
    /// 只被面板的商店页读；`fuyubot run` 不碰它（运行时永不下载，见 AGENTS 约束 61）。
    pub registry_url: Option<String>,
}

impl Plugins {
    /// 插件日志门槛：配置优先，否则 [`DEFAULT_LOG_VERBOSITY`]。
    ///
    /// **写错就报错指路**，不静默回退成默认——静默回退会让「日志全没了」的人
    /// 永远查不到真相是自己把级别名拼错了。
    pub fn log_level(&self) -> Result<LogLevel, String> {
        let Some(raw) = self
            .log_level
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(DEFAULT_LOG_VERBOSITY);
        };
        parse_log_level(raw)
    }
}

/// 面板 / 文档里按顺序展示的五档日志级别。
///
/// 顺序就是「越来越吵」：`error` → `trace`。面板的级别选择框直接拿它渲染。
pub const LOG_LEVELS: [(&str, LogLevel); 5] = [
    ("error", LogLevel::Error),
    ("warn", LogLevel::Warn),
    ("info", LogLevel::Info),
    ("debug", LogLevel::Debug),
    ("trace", LogLevel::Trace),
];

/// [`LogLevel`] 的规范名字（与 [`parse_log_level`] 认的写法一致）。
///
/// 状态文件、控制文件、面板都写这个名字；人读的 `warning` 别名只在解析时接受。
pub fn log_level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

/// 把用户写的级别名解析成 [`LogLevel`]；写错就报错指路。
///
/// `[plugins].log_level` 与 `run --log-level` 共用同一份解析，两边写法必须一致。
pub fn parse_log_level(raw: &str) -> Result<LogLevel, String> {
    match raw.to_ascii_lowercase().as_str() {
        "error" => Ok(LogLevel::Error),
        "warn" | "warning" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(format!(
            "log_level = \"{raw}\" 认不出来，只能写 error / warn / info / debug / trace"
        )),
    }
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Tdlib {
    pub path: Option<String>,
    /// TDLib 的日志详细度。**配置里没写就用 1**（框架的默认值，见
    /// [`fuyubot_tdlib::DEFAULT_LOG_VERBOSITY`]）——TDLib 自己的默认太啰嗦。
    pub log_verbosity: Option<i32>,
}

impl Tdlib {
    /// 日志详细度：配置优先，否则默认 1。**负数钳到 0**（TDLib 只认非负）。
    pub fn log_verbosity(&self) -> i32 {
        self.log_verbosity
            .unwrap_or(fuyubot_tdlib::DEFAULT_LOG_VERBOSITY)
            .max(0)
    }
}

impl Config {
    /// 插件目录（配置里没写就用 `plugins`）。
    pub fn plugin_dir(&self, root: &Path) -> PathBuf {
        root.join(self.plugins.dir.as_deref().unwrap_or("plugins"))
    }

    /// 把整个配置树序列化成 JSON（含 `[plugin.*]` 段），给宿主运行时 `install_config` 用。
    ///
    /// 插件经 ABI 的 `get_config` / `set_config` 读写的那棵树就是它。`toml::Value` 序列化成
    /// JSON 时可能失败（TOML 里的日期时间 / 内联表边缘情况），失败就返回 `Err` 由调用方报。
    pub fn to_json(&self) -> Result<serde_json::Value, String> {
        let toml_value =
            toml::Value::try_from(self).map_err(|e| format!("配置序列化成 TOML 失败: {e}"))?;
        serde_json::to_value(toml_value).map_err(|e| format!("TOML 转 JSON 失败: {e}"))
    }
}

/// 读配置；文件不存在就给一份默认值（**不是错误**：不配也能起）。
pub fn load_config(path: &Path) -> Result<Config, String> {
    if !path.is_file() {
        return Ok(Config::default());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{} 不是合法配置: {e}", path.display()))
}

// ── 凭据（api_id / api_hash）的落盘 ────────────────────────────────
//
// `run` 每次冷启动都要把 api_id 与 api_hash 随 `setTdlibParameters` 发给 TDLib（session
// 还有效也照样要发），所以「交互问一次就记住」是刚需——否则每次启动都得手输两遍。
// 记住的位置就是 `fuyubot.toml` 的 `[telegram]` 段（约束 41）。

/// 读配置文件里已保存的 `[telegram].api_hash`。
///
/// ⚠️ **故意不进 [`Config`]**：`Config::to_json()` 的整棵树会交给宿主运行时，而插件经
/// ABI 的 `get_config` 能读**任意**段——把凭据放进那棵树等于发给每个插件。所以这个键
/// 只由 CLI 自己按文件读。插件写配置只动 `plugin-config/`、碰不到本文件，凭据因此安全。
pub fn load_api_hash(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let saved: SavedCredentials = toml::from_str(&text).ok()?;
    saved
        .telegram
        .and_then(|section| section.api_hash)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 只解析凭据那两个键的小结构（[`load_api_hash`] 用）。
#[derive(serde::Deserialize)]
struct SavedCredentials {
    telegram: Option<SavedTelegram>,
}

#[derive(serde::Deserialize)]
struct SavedTelegram {
    api_hash: Option<String>,
}

/// 把**交互问出来**的凭据写回配置文件，下次不用再问。
///
/// 只写传进来的那几个：`None` = 这次没问过（值来自环境变量或配置），**不动文件**——
/// 「我没让它改配置它却改了」比多问一次更让人困惑。返回**有没有真的改文件**。
///
/// ⚠️ 只做**文本级替换**，不用 `toml::to_string` 全量重写：后者会把文件里所有注释
/// （含每个 `[plugin.*]` 段的说明）冲掉，代价远大于收益。这里是「键在就改值、
/// 不在就插一行、段不在就补一段」。
///
/// 写盘失败返回 `Err`：**调用方只许警告，不许因此起不来**。
pub fn remember_credentials(
    path: &Path,
    api_id: Option<i64>,
    api_hash: Option<&str>,
) -> Result<bool, String> {
    if api_id.is_none() && api_hash.is_none() {
        return Ok(false);
    }
    let original =
        std::fs::read_to_string(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
    let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
    let mut changed = false;
    if let Some(id) = api_id {
        changed |= set_toml_key(&mut lines, "telegram", "api_id", &id.to_string());
    }
    if let Some(hash) = api_hash {
        changed |= set_toml_key(&mut lines, "telegram", "api_hash", &quote_toml_string(hash));
    }
    if !changed {
        return Ok(false);
    }
    write_toml_lines(path, &original, lines)?;
    Ok(true)
}

/// 单个配置项写回：在 `[section]` 段里把 `key` 设成 TOML 字面量 `value`。
///
/// 面板的「编辑单个配置项」走这里。和 [`remember_credentials`] 一样是**文本级替换**：
/// 保留注释、顺序、CRLF，只改这一段值（键/段不存在就补）。值可以跨行（多行数组 /
/// 内联表）：整段折叠成一行，值里的注释原样搬到新值下面，绝不把后面的段吞掉。
/// `value` 必须是**已校验并渲染好的 TOML 字面量**（字符串带引号，数字原样），
/// 调用方负责类型校验。写回前会先把整份文件重新解析一遍：段名 / 值跟现有内容
/// 冲突（例如给内联表再补一个同名段）时返回 `Err`，**不动原文件**。
pub fn set_config_value(
    path: &Path,
    section: &str,
    key: &str,
    value: &str,
) -> Result<bool, String> {
    if section.contains(['\n', '\r']) || key.contains(['\n', '\r']) {
        return Err("配置段名 / 键名里不能有换行".to_owned());
    }
    let original =
        std::fs::read_to_string(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
    let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
    if !set_toml_key(&mut lines, section, key, value) {
        return Ok(false);
    }
    // 写回前先自己解析一遍：段名 / 值与现有文件冲突时宁可报错，也别把配置改成半截。
    let candidate = render_toml_lines(&original, &lines);
    toml::from_str::<toml::Value>(&candidate)
        .map_err(|error| format!("改完的配置不是合法 TOML：{error}"))?;
    write_config_file(path, &candidate)?;
    Ok(true)
}

/// 把改过的行拼回文本，保留原文件的 EOL 与末尾换行。
fn render_toml_lines(original: &str, lines: &[String]) -> String {
    // ⚠️ `lines()` 会把 `\r\n` 的 `\r` 也吃掉：Windows 上（或 git 开了 autocrlf）用户的
    // 配置是 CRLF，照 `join("\n")` 写回去等于把整个文件静默转成 LF——diff 里全是红。
    let eol = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut text = lines.join(eol);
    // `lines()` 把末尾换行吃掉了，写回去要补上——否则下一次回写会把最后两行粘成一行。
    if original.ends_with('\n') {
        text.push_str(eol);
    }
    text
}

/// 把改过的行写回文件，保留原文件的 EOL 与末尾换行。
fn write_toml_lines(path: &Path, original: &str, lines: Vec<String>) -> Result<(), String> {
    let text = render_toml_lines(original, &lines);
    write_config_file(path, &text)
}

/// `[telegram] # 注释` 也算 `[telegram]` 段头（注释不能把段变成「不存在」）。
fn section_header_is(line: &str, header: &str) -> bool {
    let text = line.trim();
    text.split('#').next().unwrap_or("").trim() == header
}

/// 段结束位置：下一个**顶层** `[…]` 段头，或文件末尾。
///
/// 不能只看「行首是 `[`」：多行数组的元素也可能写成 `["a", "b"]` / `[1, 2]` 开头，
/// 那会被误认成段头、把后面的键划到段外。用 [`value_span`] 跳过整个值，只有键行之外
/// 的 `[…]` 才算段头。
fn section_end(lines: &[String], start: usize) -> usize {
    let mut index = start;
    while index < lines.len() {
        if lines[index].trim_start().starts_with('[') {
            return index;
        }
        let span = value_span(lines, index);
        index = if span.last > index {
            span.last + 1
        } else {
            index + 1
        };
    }
    lines.len()
}

/// 在 `[section]` 段里把 `key` 设成 `value`（原地改 `lines`），返回有没有改动。
///
/// 段里可能有提到同名键的**注释行**（模板里就有），匹配时跳过以 `#` 开头的行；
/// `api_id_extra` 这种前缀相同的键也要放过（看 `key` 后面是不是紧跟 `=`）。
fn set_toml_key(lines: &mut Vec<String>, section: &str, key: &str, value: &str) -> bool {
    let header = format!("[{section}]");
    let start = match lines
        .iter()
        .position(|line| section_header_is(line, &header))
    {
        Some(index) => index + 1,
        // 连这一段都没有：补一段到文件末尾。
        None => {
            while lines.last().is_some_and(|line| line.trim().is_empty()) {
                lines.pop();
            }
            lines.push(String::new());
            lines.push(header);
            lines.push(format!("{key} = {value}"));
            return true;
        }
    };
    // 段的边界：下一个顶层 `[…]` 行，或文件末尾。
    let end = section_end(lines, start);

    for index in start..end {
        let trimmed = lines[index].trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(key) else {
            continue;
        };
        if !rest.trim_start().starts_with('=') {
            continue;
        }
        let span = value_span(&lines, index);
        if span.last == index {
            let rendered = format!("{key} = {value}{}", trailing_comment(&lines[index]));
            if lines[index] == rendered {
                return false;
            }
            lines[index] = rendered;
            return true;
        }
        // 跨行值（多行数组 / 内联表）：整段换成一行，并把值里的注释**原样搬到下面**。
        // 注释内容一个字不丢；位置只能变——元素数量一变，逐项对应就无从谈起了。
        let mut replacement = vec![format!("{key} = {value}{}", span.first_comment)];
        replacement.extend(span.comments);
        lines.splice(index..=span.last, replacement);
        return true;
    }

    // 键不在段里：插到段末尾（跳过尾部空行）。
    let mut insert_at = end;
    while insert_at > start && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }
    lines.insert(insert_at, format!("{key} = {value}"));
    true
}

/// 取行尾的 TOML 注释（含 `#` 前面那段空白），没有就返回空串。
///
/// 只认字符串外的 `#`：`path = "a#b" # note` 的注释是后面的 `# note`。
fn trailing_comment(line: &str) -> String {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if let Some(active) = quote {
            if active == '"' && ch == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if ch == active && !escaped {
                quote = None;
            }
            escaped = false;
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '#' => return format!(" {}", line[index..].trim_end()),
            _ => {}
        }
    }
    String::new()
}

/// 包成 TOML 基本字符串（只转义 `\` 与 `"`）。
pub fn quote_toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 一个键的**值**在文件里占的行范围，以及值里要保留的注释。
struct ValueSpan {
    /// 值结束所在的行（单行值 = 键所在行）。
    last: usize,
    /// 键所在行的行尾注释（含 `#` 前的一个空格）。
    first_comment: String,
    /// 值跨行时，后续行里的注释（整行注释保留缩进；行尾注释单独成行）。
    comments: Vec<String>,
}

/// 从 `lines[start]` 的 `=` 之后扫到值的结尾。
///
/// TOML 的值可以跨行（多行数组 / 内联表 / 三引号字符串），而且元素之间允许写注释。
/// 改这种键时只换掉第一行会把后面的元素和 `]` 留成孤儿、把配置弄坏——所以先把整段
/// 找出来，注释一并收集，[`set_toml_key`] 再把它们搬回新值下面。
///
/// 字符串里的 `#` / `[` / `]` 不算注释、不参与括号配对。扫到文件尾还没闭合就当单行
/// （文件本来就不是合法 TOML，面板也不会走到这里）。
fn value_span(lines: &[String], start: usize) -> ValueSpan {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Basic,
        Literal,
        MultiBasic,
        MultiLiteral,
    }

    let mut span = ValueSpan {
        last: start,
        first_comment: String::new(),
        comments: Vec::new(),
    };
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut depth = 0i32;

    for (offset, line) in lines[start..].iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let mut index = if offset == 0 {
            chars
                .iter()
                .position(|ch| *ch == '=')
                .map_or(0, |at| at + 1)
        } else {
            0
        };
        let mut comment_at = None;
        while index < chars.len() {
            let ch = chars[index];
            match quote {
                Quote::None => match ch {
                    '"' => {
                        if chars.get(index + 1) == Some(&'"') && chars.get(index + 2) == Some(&'"')
                        {
                            quote = Quote::MultiBasic;
                            index += 3;
                            continue;
                        }
                        quote = Quote::Basic;
                    }
                    '\'' => {
                        if chars.get(index + 1) == Some(&'\'')
                            && chars.get(index + 2) == Some(&'\'')
                        {
                            quote = Quote::MultiLiteral;
                            index += 3;
                            continue;
                        }
                        quote = Quote::Literal;
                    }
                    '[' | '{' => depth += 1,
                    ']' | '}' => depth -= 1,
                    '#' => {
                        comment_at = Some(index);
                        break;
                    }
                    _ => {}
                },
                Quote::Basic => {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quote = Quote::None;
                    }
                }
                Quote::Literal => {
                    if ch == '\'' {
                        quote = Quote::None;
                    }
                }
                Quote::MultiBasic => {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"'
                        && chars.get(index + 1) == Some(&'"')
                        && chars.get(index + 2) == Some(&'"')
                    {
                        quote = Quote::None;
                        index += 3;
                        continue;
                    }
                }
                Quote::MultiLiteral => {
                    if ch == '\''
                        && chars.get(index + 1) == Some(&'\'')
                        && chars.get(index + 2) == Some(&'\'')
                    {
                        quote = Quote::None;
                        index += 3;
                        continue;
                    }
                }
            }
            index += 1;
        }

        if let Some(at) = comment_at {
            let text: String = chars[at..].iter().collect();
            if offset == 0 {
                span.first_comment = format!(" {}", text.trim_end());
            } else {
                let before: String = chars[..at].iter().collect();
                if before.trim().is_empty() {
                    // 整行注释：连缩进一起原样留下。
                    span.comments
                        .push(format!("{before}{text}").trim_end().to_owned());
                } else {
                    // 值后面的行尾注释：单独成行，缩进跟这一行对齐。
                    let indent: String =
                        before.chars().take_while(|ch| ch.is_whitespace()).collect();
                    span.comments.push(format!("{indent}{}", text.trim_end()));
                }
            }
        }

        span.last = start + offset;
        if quote == Quote::None && depth <= 0 {
            return span;
        }
    }

    // 扫到文件尾都没闭合：文件本来就不是合法 TOML。按单行处理，绝不把后面的段吞掉。
    span.last = start;
    span.first_comment.clear();
    span.comments.clear();
    span
}
// ── config list ────────────────────────────────────────────────────

/// `fuyubot config` 的几行输出：**生效值**（缺省已填好），外加这份文件到底在哪。
///
/// 纯函数，不碰进程环境（唯一例外是读 `config_path` 里那个凭据键）。文件不存在时如实说
/// 「全是默认值」——`run` 本来就是按默认值起的。
/// 用户 id 列表的展示：空表写成「（空）」，否则逗号分隔。
fn format_ids(ids: &[i64]) -> String {
    if ids.is_empty() {
        "（空）".to_owned()
    } else {
        ids.iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn config_lines(config_path: &Path, root: &Path, config: &Config) -> Vec<String> {
    let missing = if config_path.is_file() {
        ""
    } else {
        "（不存在，下面全是默认值）"
    };
    let account = match config.telegram.account_type() {
        Ok(kind) => kind.as_str().to_owned(),
        // 写错就**把原因原样报出来**：这正是「我改的那行生效了吗」要回答的东西。
        Err(e) => format!("⚠️ {e}"),
    };
    let tdlib_path = config
        .tdlib
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("自动（TDLIB_TDJSON_PATH → 自带 → exe 同级 → 系统库 → 裸库名）");
    let whitelist = if config.command.whitelist.is_empty() {
        "（空，不额外限制）".to_owned()
    } else {
        config
            .command
            .whitelist
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let prefixes = match config.command.prefixes() {
        Some(prefixes) => prefixes.join(", "),
        None => "按账号默认（bot \"/\"、user \".\"）".to_owned(),
    };
    let whitelist_only = if config.command.whitelist_only() {
        "true（只认自己 + whitelist）".to_owned()
    } else {
        "false（⚠️ 已关闭：任何人都能触发，不建议）".to_owned()
    };
    let owners = format_ids(&config.roles.owners);
    let admins = format_ids(&config.roles.admins);
    let cache_enabled = if config.cache.enabled.unwrap_or(true) {
        "true".to_owned()
    } else {
        "false（不读写缓存）".to_owned()
    };
    let cache_capacity = config.cache.capacity.unwrap_or(DEFAULT_CACHE_CAPACITY);
    let mut lines = vec![
        format!("配置文件  {}{missing}", config_path.display()),
        format!("项目根    {}", root.display()),
        format!(
            "[bot]       name         = {}",
            config.bot.name.as_deref().unwrap_or("(未设置)")
        ),
        format!("[telegram]  account_type = {account}"),
        format!(
            "[telegram]  api_id       = {}",
            config.telegram.api_id(&config.bot)
        ),
        // 凭据**不回显**：只说有没有存（它现在是明文躺在文件里的，终端再打一遍没意义）。
        format!(
            "[telegram]  api_hash     = {}",
            match load_api_hash(config_path) {
                Some(_) => "已保存在本文件（不回显）".to_owned(),
                None => "未保存（TG_API_HASH 环境变量，或 run 交互输入后自动写回）".to_owned(),
            }
        ),
        format!(
            "[telegram]  data_dir     = {}",
            config.telegram.data_dir(root).display()
        ),
        format!("[command]   prefix       = {prefixes}"),
        format!("[command]   whitelist    = {whitelist}"),
        format!("[command]   whitelist_only = {whitelist_only}"),
        format!("[roles]     owners       = {owners}"),
        format!("[roles]     admins       = {admins}"),
        format!("[cache]     enabled      = {cache_enabled}"),
        format!("[cache]     capacity     = {cache_capacity}（每种类型）"),
        format!(
            "[plugins]   dir          = {}",
            config.plugin_dir(root).display()
        ),
        format!(
            "[plugins]   log_level    = {}",
            config
                .plugins
                .log_level
                .as_deref()
                .unwrap_or("info（默认）")
        ),
        format!(
            "[plugins]   registry_url = {}",
            config
                .plugins
                .registry_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("未配置（商店不可用）")
        ),
        format!("[tdlib]     path         = {tdlib_path}"),
        format!(
            "[tdlib]     log_verbosity= {}",
            config.tdlib.log_verbosity()
        ),
        // Bot Token / 手机号 / 两步验证密码**仍然不落盘**：session 有效后它们用不上，
        // 落了只是多一份明文。api_hash 是例外（每次冷启动都要发，见上面那行的注释）。
        "凭据        Bot Token / 手机号 / 密码不落盘（TG_BOT_TOKEN 或交互输入）".to_owned(),
    ];
    // 插件各自的配置段：把 `plugin` 表按「点路径」展平成 `[plugin.fuyubot.manage]` 这样一行一个。
    // 插件名是点分的，`[plugin.fuyubot.manage]` 解析成嵌套表 `{ fuyubot: { manage: … } }`；
    // 这里递归下钻，每个「叶子表」（内容是一组键值、不再含子表的那层）打印成一行。
    let mut plugin_lines = Vec::new();
    flatten_plugin_sections(&config.plugin, "plugin", &mut plugin_lines);
    if plugin_lines.is_empty() {
        lines.push("插件配置    无（没有 [plugin.*] 段）".to_owned());
    } else {
        lines.extend(plugin_lines);
    }
    lines
}

/// 递归展平 `plugin` 表：一个「叶子表」= 一段插件的配置，一行 `[plugin.<路径>] = {…}`。
fn flatten_plugin_sections(value: &toml::Value, prefix: &str, out: &mut Vec<String>) {
    let Some(table) = value.as_table() else {
        out.push(format!("[{prefix}] = {value}"));
        return;
    };
    // 有叶子键（非表）的就是一个「插件段」，把它的键值序列化成 JSON 摘要。
    let leaves: Vec<(&String, &toml::Value)> =
        table.iter().filter(|(_, v)| !v.is_table()).collect();
    if !leaves.is_empty() {
        let summary = leaves
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!("[{prefix}] = {{ {summary} }}"));
    }
    // 子表继续下钻（可能代表更深一层的插件名，或插件段里的嵌套表）。
    for (key, child) in table {
        if child.is_table() {
            flatten_plugin_sections(child, &format!("{prefix}.{key}"), out);
        }
    }
}

// ── 初始化（`fuyubot init` / Android `config_init`）────────────────

/// 渲染模板：`__NAME__` / `__API_ID__` 两个占位。
///
/// `api_hash` **不在这里填**：模板里是空占位，真值由调用方随后用
/// [`remember_credentials`] 写回（与 CLI 的 `run` 交互问过之后同一条路）。
pub fn render_template(name: &str, api_id: i64) -> String {
    CONFIG_TEMPLATE
        .replace("__NAME__", name)
        .replace("__API_ID__", &api_id.to_string())
}

/// 一次「建 / 改配置文件」的请求。字段都是 `Option`：`None` = 这次不动那个键。
#[derive(Debug, Default, Clone)]
pub struct InitRequest {
    /// `[bot].name`；缺省用 root 的目录名（再不行 `fuyubot`）。
    pub name: Option<String>,
    /// `[telegram].api_id`。文件不存在时用来渲染模板，存在时改这一行。
    pub api_id: Option<i64>,
    /// `[telegram].api_hash`。**只写这一行**，不进 [`Config`] 树（约束 41）。
    pub api_hash: Option<String>,
    /// `[telegram].account_type`（`bot` / `user`）。
    pub account_type: Option<String>,
    /// `[telegram].data_dir`。
    pub data_dir: Option<String>,
    /// `[plugins].dir`。
    pub plugin_dir: Option<String>,
    /// `[plugins].log_level`（写规范名字，别名先归一化）。
    pub log_level: Option<String>,
    /// `[tdlib].log_verbosity`（负数钳到 0）。
    pub tdlib_log_verbosity: Option<i32>,
    /// `[tdlib].path`。
    pub tdlib_path: Option<String>,
    /// `true` = 整份重写模板（**会丢注释与 `[plugin.*]` 段**，只在用户明确要求时用）。
    pub overwrite: bool,
}

/// 建或改 `<root>/fuyubot.toml`，返回文件路径。
///
/// 两种情形：
/// - 文件不存在（或 `overwrite`）：按模板渲染一份，再填请求里给了的键；
/// - 文件已存在：**文本级**只改请求里给了的键（保留注释、顺序、其它段），
///   与面板的 [`set_config_value`] 是同一条路径。
pub fn init_config(root: &Path, request: &InitRequest) -> Result<PathBuf, String> {
    let path = root.join(CONFIG_FILE);
    if request.overwrite || !path.exists() {
        let name = request
            .name
            .clone()
            .or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "fuyubot".to_owned());
        let rendered = render_template(name.trim(), request.api_id.unwrap_or(0));
        write_config_file(&path, &rendered)?;
    }

    // 下面每条都是「键在就改值、不在就插一行」：文件是刚渲染的还是用户手改过的，结果一样。
    if request.api_id.is_some() || request.api_hash.is_some() {
        remember_credentials(&path, request.api_id, request.api_hash.as_deref())?;
    }
    if let Some(raw) = request.account_type.as_deref() {
        // 写错要在这里报出来，别等登录才说「account_type 认不出来」。
        Telegram {
            account_type: Some(raw.to_owned()),
            ..Telegram::default()
        }
        .account_type()?;
        set_config_value(
            &path,
            "telegram",
            "account_type",
            &quote_toml_string(raw.trim()),
        )?;
    }
    if let Some(raw) = request.data_dir.as_deref() {
        set_config_value(
            &path,
            "telegram",
            "data_dir",
            &quote_toml_string(raw.trim()),
        )?;
    }
    if let Some(raw) = request.plugin_dir.as_deref() {
        set_config_value(&path, "plugins", "dir", &quote_toml_string(raw.trim()))?;
    }
    if let Some(raw) = request.log_level.as_deref() {
        let level = parse_log_level(raw)?;
        set_config_value(
            &path,
            "plugins",
            "log_level",
            &quote_toml_string(log_level_name(level)),
        )?;
    }
    if let Some(raw) = request.tdlib_log_verbosity {
        set_config_value(&path, "tdlib", "log_verbosity", &raw.max(0).to_string())?;
    }
    if let Some(raw) = request.tdlib_path.as_deref() {
        set_config_value(&path, "tdlib", "path", &quote_toml_string(raw.trim()))?;
    }
    Ok(path)
}

/// 写整份文件（渲染模板用）：父目录不存在就建。
fn write_config_file(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了 {}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("写不了 {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个干净目录（`target/test-tmp` 下，按 pid 分开：并行跑也不打架）。
    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-app")
            .join(name)
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `[plugin.<插件名>]` 段能解析回来，并能序列化成给宿主运行时的 JSON（点分插件名走嵌套路径）。
    #[test]
    fn plugin_sections_parse_and_round_trip_to_json() {
        let root = temp("plugin-config");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "[bot]\nname = \"x\"\n\n[plugin.fuyubot.manage]\nreply_prefix = \"已安装插件：\"\n\n[plugin.my.weather]\ncity = \"上海\"\n",
        )
        .unwrap();
        let config = load_config(&path).unwrap();

        // `plugin` 是一棵嵌套表：`fuyubot` → `manage` → `{ reply_prefix }`。
        let plugin = config.plugin.as_table().unwrap();
        assert!(plugin.contains_key("fuyubot"), "{plugin:?}");
        assert!(plugin.contains_key("my"), "{plugin:?}");

        // 序列化成 JSON 后，宿主的 `get_config("fuyubot.manage", "reply_prefix")` 能下钻到。
        let json = config.to_json().unwrap();
        let prefix = json
            .pointer("/plugin/fuyubot/manage/reply_prefix")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert_eq!(prefix, "已安装插件：");
        assert_eq!(
            json.pointer("/plugin/my/weather/city")
                .and_then(serde_json::Value::as_str)
                .unwrap(),
            "上海"
        );

        // 配置展示里要把段摊开成 `[plugin.fuyubot.manage]` 这样的可读行。
        let lines = config_lines(&path, &root, &config).join("\n");
        assert!(lines.contains("[plugin.fuyubot.manage]"), "{lines}");
        assert!(lines.contains("[plugin.my.weather]"), "{lines}");
    }

    /// `[cache]` 段能解析、进配置树（宿主据此配置缓存），也能在 `config` 里看到生效值。
    #[test]
    fn cache_section_parses_and_round_trips() {
        let root = temp("cache-config");
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, "[cache]\nenabled = false\ncapacity = 50\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.cache.enabled, Some(false));
        assert_eq!(config.cache.capacity, Some(50));

        let json = config.to_json().unwrap();
        assert_eq!(
            json.pointer("/cache/enabled")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            json.pointer("/cache/capacity")
                .and_then(serde_json::Value::as_u64),
            Some(50)
        );

        let lines = config_lines(&path, &root, &config).join("\\n");
        assert!(
            lines.contains("[cache]     enabled      = false（不读写缓存）"),
            "{lines}"
        );
        assert!(
            lines.contains("[cache]     capacity     = 50（每种类型）"),
            "{lines}"
        );
    }

    /// `[command]` 段能解析、进配置树（宿主据此装闸门），也能在 `config` 里看到生效值。
    #[test]
    fn command_section_parses_and_round_trips() {
        let root = temp("command-config");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "[command]\nprefix = [\"!\", \"。\"]\nwhitelist = [7, 8]\nwhitelist_only = false\n",
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(
            config.command.prefixes(),
            Some(&["!".to_owned(), "。".to_owned()][..])
        );
        assert_eq!(config.command.whitelist, vec![7, 8]);
        assert!(!config.command.whitelist_only());

        let json = config.to_json().unwrap();
        assert_eq!(
            json.pointer("/command/prefix/0")
                .and_then(serde_json::Value::as_str),
            Some("!")
        );
        assert_eq!(
            json.pointer("/command/prefix/1")
                .and_then(serde_json::Value::as_str),
            Some("。")
        );
        assert_eq!(
            json.pointer("/command/whitelist/1")
                .and_then(serde_json::Value::as_i64),
            Some(8)
        );
        assert_eq!(
            json.pointer("/command/whitelist_only")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );

        let lines = config_lines(&path, &root, &config).join("\n");
        assert!(
            lines.contains("[command]   prefix       = !, 。"),
            "{lines}"
        );
        assert!(lines.contains("[command]   whitelist    = 7, 8"), "{lines}");
        assert!(
            lines.contains("whitelist_only = false（⚠️ 已关闭：任何人都能触发，不建议）"),
            "{lines}"
        );

        // 没写时按账号默认（模板也必须说清楚）。模板**不能钉死** `prefix`：
        // 一旦写进配置就是整套替换，bot 会连 `/` 都不认。
        assert_eq!(Command::default().prefixes(), None);
        assert!(
            Command::default().whitelist_only(),
            "user 账户默认白名单模式"
        );
        assert!(
            CONFIG_TEMPLATE.contains("不建议关闭"),
            "模板必须提醒不要关白名单模式：\n{CONFIG_TEMPLATE}"
        );
        assert!(
            Config::default()
                .to_json()
                .unwrap()
                .pointer("/command/prefix")
                .is_none(),
            "没写命令头时配置树里不该出现 prefix（宿主按账号默认）"
        );
        let lines = config_lines(&path, &root, &Config::default()).join("\n");
        assert!(
            lines.contains("按账号默认（bot \"/\"、user \".\"）"),
            "{lines}"
        );
        let active_lines = CONFIG_TEMPLATE
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !active_lines.contains("prefix ="),
            "模板不能把命令头钉死，否则账号默认值会被整套替换：\n{CONFIG_TEMPLATE}"
        );
    }

    /// 旧写法（`prefix = "!"` 字符串）只接受数组，读配置时必须报错。
    #[test]
    fn a_string_prefix_is_rejected() {
        let root = temp("command-string-prefix");
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, "[command]\nprefix = \"!\"\n").unwrap();
        let error = load_config(&path).unwrap_err();
        assert!(error.contains("prefix"), "{error}");
    }

    /// 账号类型写错必须**报错指路**，不能静默当 bot——否则用户会拿着错的登录方式
    /// 一直等不到验证码，而错误信息完全不指向配置。
    #[test]
    fn a_bogus_account_type_is_rejected_with_a_hint() {
        let telegram = Telegram {
            account_type: Some("robot".to_owned()),
            ..Telegram::default()
        };
        let error = telegram.account_type().unwrap_err();
        assert!(error.contains("robot"), "报错要带上写错的值：{error}");
        assert!(
            error.contains("bot") && error.contains("user"),
            "报错要说清只能写哪两个：{error}"
        );
        // 大写/带空格不算错（手写配置很常见），`user` 要真的认成 user。
        let telegram = Telegram {
            account_type: Some(" User ".to_owned()),
            ..Telegram::default()
        };
        assert_eq!(
            telegram.account_type().unwrap(),
            fuyubot_tdlib::AccountType::User
        );
    }

    /// 日志详细度的默认值与钳位：**没配就是 1**（TDLib 自己的默认太啰嗦），
    /// 负数当 0（TDLib 只认非负，别把负值原样丢过去）。
    #[test]
    fn log_verbosity_defaults_to_one_and_clamps_negatives() {
        assert_eq!(Tdlib::default().log_verbosity(), 1);
        assert_eq!(
            Tdlib {
                path: None,
                log_verbosity: Some(5)
            }
            .log_verbosity(),
            5
        );
        assert_eq!(
            Tdlib {
                path: None,
                log_verbosity: Some(-3)
            }
            .log_verbosity(),
            0
        );
        // 模板里写出来的那个值也要等于默认值，否则「默认 1」只存在于代码注释里。
        let rendered = CONFIG_TEMPLATE;
        assert!(
            rendered.contains("log_verbosity = 1"),
            "模板里的 log_verbosity 必须与默认值一致：\n{rendered}"
        );
    }

    /// 插件日志门槛：没配就是 `Info`（默认安静），写错级别名要**报错指路**。
    #[test]
    fn plugin_log_level_defaults_to_info_and_rejects_unknown_names() {
        assert_eq!(Plugins::default().log_level().unwrap(), LogLevel::Info);

        for (raw, expected) in [
            ("error", LogLevel::Error),
            ("warn", LogLevel::Warn),
            ("warning", LogLevel::Warn),
            ("INFO", LogLevel::Info),
            (" debug ", LogLevel::Debug),
            ("trace", LogLevel::Trace),
        ] {
            assert_eq!(
                Plugins {
                    dir: None,
                    log_level: Some(raw.to_owned()),
                    registry_url: None,
                }
                .log_level()
                .unwrap(),
                expected
            );
        }

        let error = Plugins {
            dir: None,
            log_level: Some("verbose".to_owned()),
            registry_url: None,
        }
        .log_level()
        .unwrap_err();
        assert!(error.contains("verbose"), "报错要带上写错的值：{error}");
        assert!(
            error.contains("error") && error.contains("trace"),
            "报错要说清能写哪几个：{error}"
        );

        // 规范名字与解析必须互为逆运算：状态文件 / 控制文件写的都是规范名字。
        for (name, level) in LOG_LEVELS {
            assert_eq!(parse_log_level(name).unwrap(), level);
            assert_eq!(log_level_name(level), name);
        }

        // 模板里写出来的那个值必须等于默认值，否则「默认 info」只存在于注释里。
        assert!(
            CONFIG_TEMPLATE.contains("log_level = \"info\""),
            "模板里的 log_level 必须与默认值一致：\n{CONFIG_TEMPLATE}"
        );
    }

    /// `init` 手上没有 api_hash，所以它只留下**空占位**——真值由 `run` 问过之后回写。
    #[test]
    fn init_leaves_the_api_hash_empty() {
        let root = temp("init-no-secret");
        let path = init_config(
            &root,
            &InitRequest {
                name: Some("mybot".to_owned()),
                api_id: Some(12345),
                ..InitRequest::default()
            },
        )
        .unwrap();
        // 空串当「没存」：不能让「api_hash = ""」被当成有效凭据发给 TDLib。
        assert_eq!(load_api_hash(&path), None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("__API_HASH__"),
            "模板占位符没被换掉：\n{text}"
        );
    }

    /// 面板改单个配置项：只动一行，注释和 CRLF 都保留；键/段不存在就补。
    #[test]
    fn set_config_value_keeps_comments_and_line_endings() {
        let root = temp("set-config-value");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "# 顶上的注释\r\n\
             [plugins] # 段注释\r\n\
             # 别丢的注释\r\n\
             dir = \"plugins\"\r\n\
             log_level = \"info\" # 行尾注释\r\n",
        )
        .unwrap();

        assert!(
            set_config_value(&path, "plugins", "log_level", "\"debug\"").unwrap(),
            "值变了"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 顶上的注释"), "{text:?}");
        assert!(text.contains("# 别丢的注释"), "{text:?}");
        assert!(text.contains("log_level = \"debug\""), "{text:?}");
        assert!(text.contains("# 行尾注释"), "行尾注释要保留：{text:?}");
        assert_eq!(text.matches("[plugins]").count(), 1, "段头别重复：{text:?}");
        assert!(text.contains("\r\n"), "CRLF 不能被转成 LF：{text:?}");

        // 同值再写 = 没改动。
        assert!(!set_config_value(&path, "plugins", "log_level", "\"debug\"").unwrap());

        // 段里的键不存在：插到段内；段本身不存在：补一段。
        assert!(set_config_value(&path, "tdlib", "log_verbosity", "3").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[tdlib]"), "{text:?}");
        assert!(text.contains("log_verbosity = 3"), "{text:?}");
        assert_eq!(load_config(&path).unwrap().tdlib.log_verbosity(), 3);
    }

    /// 跨行值（多行数组）也能改：整段换成一行，值里的注释一个字不丢，配置仍然合法。
    #[test]
    fn editing_a_multiline_array_keeps_every_comment() {
        let root = temp("multiline-array-edit");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "[command]\n\
             # 命令头说明\n\
             prefix = [\n\
             \x20 \".\",  # 点\n\
             \x20 \"!\",  # 感叹\n\
             ]  # 命令头\n\
             whitelist = []  # 白名单\n",
        )
        .unwrap();

        assert!(
            set_config_value(&path, "command", "prefix", "[\"/\", \"。\"]").unwrap(),
            "多行数组被折叠成一行也算改动"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        for comment in ["# 命令头说明", "# 点", "# 感叹", "# 命令头", "# 白名单"] {
            assert!(text.contains(comment), "丢了注释 {comment}：\n{text}");
        }
        assert!(text.contains("prefix = [\"/\", \"。\"]"), "{text}");
        assert_eq!(
            load_config(&path).unwrap().command.prefix,
            Some(vec!["/".to_owned(), "。".to_owned()]),
            "{text}"
        );

        // 再写一次同样的值：现在已经是单行，不该再动文件。
        assert!(!set_config_value(&path, "command", "prefix", "[\"/\", \"。\"]").unwrap());
    }

    /// 单行数组的行尾注释也不能丢。
    #[test]
    fn editing_a_single_line_array_keeps_the_trailing_comment() {
        let root = temp("single-line-array-edit");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "[command]\nprefix = [\".\", \"!\"]  # 命令头\nwhitelist = []\n",
        )
        .unwrap();

        assert!(set_config_value(&path, "command", "prefix", "[\"/\", \"。\"]").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("prefix = [\"/\", \"。\"] # 命令头"),
            "行尾注释要留在同一行：\n{text}"
        );
        assert_eq!(
            load_config(&path).unwrap().command.prefix,
            Some(vec!["/".to_owned(), "。".to_owned()])
        );
    }

    /// 嵌套数组的元素也以 `[` 开头：它们不是段头，后面的键仍属于这一段。
    #[test]
    fn multiline_nested_array_does_not_look_like_a_section_header() {
        let root = temp("nested-array-section-end");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "[plugin.demo]\nmatrix = [\n  [1, 2],\n  [3, 4],\n]\nname = \"d\"\n",
        )
        .unwrap();

        assert!(
            set_config_value(&path, "plugin.demo", "name", "\"renamed\"").unwrap(),
            "name 在同一个段里，应该能改"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[plugin.demo]").count(), 1, "{text}");
        assert!(text.contains("name = \"renamed\""), "{text}");
        assert_eq!(
            load_config(&path)
                .unwrap()
                .plugin
                .get("demo")
                .and_then(|table| table.get("name"))
                .and_then(|value| value.as_str()),
            Some("renamed")
        );
    }

    /// 插件段按点路径写：`[plugin.fuyubot.manage]` 与内置段是同一条文本级路径。
    #[test]
    fn plugin_sections_are_editable_by_dotted_path() {
        let root = temp("plugin-section-edit");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "# 插件段\n[plugin.fuyubot.manage]\nreply_prefix = \"已安装：\"\ntoken = \"t\"\n",
        )
        .unwrap();

        assert!(set_config_value(&path, "plugin.fuyubot.manage", "token", "\"new\"").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 插件段"), "{text}");
        assert!(text.contains("token = \"new\""), "{text}");
        assert_eq!(
            load_config(&path)
                .unwrap()
                .plugin
                .get("fuyubot")
                .and_then(|table| table.get("manage"))
                .and_then(|table| table.get("token"))
                .and_then(|value| value.as_str()),
            Some("new")
        );
    }

    /// 写完会让文件变成非法 TOML 的改动必须被拒绝——宁可报错，也不能把配置改坏。
    #[test]
    fn a_write_that_would_break_toml_is_rejected() {
        let root = temp("broken-write");
        let path = root.join(CONFIG_FILE);
        let original = "[plugin]\ndemo = { name = \"d\" }\n";
        std::fs::write(&path, original).unwrap();

        // `demo` 已经是内联表：再补一个 `[plugin.demo]` 段就是重复定义。
        let error = set_config_value(&path, "plugin.demo", "name", "\"x\"").unwrap_err();
        assert!(error.contains("不是合法 TOML"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// 在**真模板**上改每一行可编辑的键：所有注释必须原样留下。
    #[test]
    fn editing_every_template_line_keeps_all_comments() {
        let root = temp("template-line-edits");
        let path = root.join(CONFIG_FILE);
        let template = CONFIG_TEMPLATE
            .replace("__NAME__", "demo")
            .replace("__API_ID__", "1");
        std::fs::write(&path, &template).unwrap();

        let edits = [
            ("bot", "name", "\"renamed\""),
            ("telegram", "account_type", "\"user\""),
            ("telegram", "api_id", "12345"),
            ("telegram", "data_dir", "\"data2\""),
            ("telegram", "api_hash", "\"hash\""),
            ("plugins", "dir", "\"plugins2\""),
            ("plugins", "log_level", "\"debug\""),
            ("tdlib", "path", "\"tdjson\""),
            ("tdlib", "log_verbosity", "3"),
            ("command", "whitelist_only", "false"),
        ];
        let comments = template
            .lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .map(str::to_owned)
            .collect::<Vec<_>>();

        for (section, key, value) in edits {
            assert!(
                set_config_value(&path, section, key, value).unwrap(),
                "[{section}].{key} 该有改动"
            );
            let text = std::fs::read_to_string(&path).unwrap();
            for comment in &comments {
                assert!(
                    text.lines()
                        .any(|line| line.trim_end() == comment.trim_end()),
                    "改 [{section}].{key} 后丢了注释 {comment:?}：\n{text}"
                );
            }
            load_config(&path).unwrap_or_else(|error| {
                panic!("改 [{section}].{key} 后不再是合法 TOML：{error}\n{text}")
            });
        }
    }

    /// 回写凭据：**值要能读回来，注释一个字都不能丢**。
    ///
    /// 全量 `toml::to_string` 重写也能写对值，但会把用户手写的注释冲掉——那才是这份
    /// 配置里最值钱的部分（每个插件段都靠注释说明键是什么意思）。
    #[test]
    fn remembering_credentials_keeps_the_comments() {
        let root = temp("remember");
        let path = root.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "# 顶上的注释\n\
             [bot]\n\
             name = \"mybot\"\n\
             \n\
             [telegram]\n\
             # 这段的注释\n\
             account_type = \"bot\"\n\
             # api_hash 是凭据，别外传\n\
             api_id = 0\n\
             data_dir = \"data\"\n\
             \n\
             [plugins]\n\
             dir = \"plugins\"\n",
        )
        .unwrap();

        assert!(remember_credentials(&path, Some(246810), Some("deadbeef")).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        // 注释全在（含那条提到 api_hash 的）。
        assert!(text.contains("# 顶上的注释"), "{text}");
        assert!(text.contains("# 这段的注释"), "{text}");
        assert!(text.contains("# api_hash 是凭据，别外传"), "{text}");
        // 值改的是原来那一行，不是追加第二行。
        assert_eq!(
            text.lines()
                .filter(|line| line.trim_start().starts_with("api_id"))
                .count(),
            1,
            "{text}"
        );
        assert!(text.contains("api_id = 246810"), "{text}");
        assert!(text.contains("api_hash = \"deadbeef\""), "{text}");
        // 其它段没被动过。
        assert!(text.contains("dir = \"plugins\""), "{text}");

        // 读回来：api_id 走配置树，api_hash 走凭据入口。
        let config = load_config(&path).unwrap();
        assert_eq!(config.telegram.api_id(&config.bot), 246810);
        assert_eq!(load_api_hash(&path).as_deref(), Some("deadbeef"));

        // 同样的值再写一次 = 没改动（不该把文件重写一遍）。
        assert!(!remember_credentials(&path, Some(246810), Some("deadbeef")).unwrap());
        // 什么都没问 = 什么都不动。
        assert!(!remember_credentials(&path, None, None).unwrap());
    }

    /// 行尾是 CRLF 的文件回写之后**还得是 CRLF**：`lines()` 会吞掉 `\r`，照 `join("\n")`
    /// 写回去等于把整份配置静默转成 LF（git 开了 autocrlf 的 Windows 用户会看到满屏 diff）。
    #[test]
    fn remembering_keeps_crlf_line_endings() {
        let root = temp("remember-crlf");
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, "[telegram]\r\napi_id = 0\r\n").unwrap();

        assert!(remember_credentials(&path, Some(42), Some("h")).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\r\n"), "CRLF 被改成了 LF：{text:?}");
        assert!(!text.replace("\r\n", "").contains('\n'), "{text:?}");
        assert!(text.contains("api_id = 42"), "{text}");
        assert_eq!(load_api_hash(&path).as_deref(), Some("h"));
    }

    /// 只回写**问过**的那一个：api_id 来自配置时不该顺手把 hash 写进去，反之亦然。
    #[test]
    fn remembering_writes_only_what_was_asked() {
        let root = temp("remember-partial");
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, "[telegram]\napi_id = 0\n").unwrap();

        assert!(remember_credentials(&path, None, Some("only-hash")).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_id = 0"), "api_id 没问过就不该动：{text}");
        assert_eq!(load_api_hash(&path).as_deref(), Some("only-hash"));

        assert!(remember_credentials(&path, Some(111), None).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_id = 111"), "{text}");
        // hash 那行还在（这次没问，也不能顺手删）。
        assert_eq!(load_api_hash(&path).as_deref(), Some("only-hash"));
    }

    /// `[telegram]` 段整个不在（比如一份只写了插件段的配置）：补一段，不报错。
    #[test]
    fn remembering_appends_a_missing_telegram_section() {
        let root = temp("remember-no-section");
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, "[plugins]\ndir = \"plugins\"\n").unwrap();

        assert!(remember_credentials(&path, Some(7), Some("h")).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[telegram]"), "{text}");
        assert_eq!(
            load_config(&path).unwrap().telegram.api_id(&Bot::default()),
            7
        );
        assert_eq!(load_api_hash(&path).as_deref(), Some("h"));
        assert!(
            text.contains("dir = \"plugins\""),
            "原来那段不能被吃掉：{text}"
        );
    }

    /// 凭据**不进配置树**：那棵树会整棵交给宿主运行时，插件 `get_config` 能读任意段。
    #[test]
    fn the_saved_api_hash_stays_out_of_the_config_tree() {
        let root = temp("remember-not-in-tree");
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, "[telegram]\napi_id = 1\n").unwrap();
        remember_credentials(&path, None, Some("secret")).unwrap();

        let json = load_config(&path).unwrap().to_json().unwrap();
        assert!(
            json.pointer("/telegram/api_hash").is_none(),
            "凭据不能出现在给插件的配置树里：{json}"
        );
        // 空值也不算「保存过」——否则 `api_hash = ""` 会被当成凭据发给 TDLib。
        std::fs::write(&path, "[telegram]\napi_hash = \"\"\n").unwrap();
        assert_eq!(load_api_hash(&path), None);
    }

    #[test]
    fn missing_config_is_not_an_error() {
        let config = load_config(Path::new("does-not-exist.toml")).unwrap();
        assert_eq!(config.bot.api_id, None);
    }
    /// `registry_url` 要真的出现在 `fuyubot config` 的 `[plugins]` 段里。
    #[test]
    fn config_lines_show_the_store_registry() {
        let root = temp("config-registry");
        let path = root.join(CONFIG_FILE);
        let mut config = Config::default();

        config.plugins.registry_url = Some("https://example.test/index.json".to_owned());
        let lines = config_lines(&path, &root, &config).join("\n");
        assert!(
            lines.contains("registry_url = https://example.test/index.json"),
            "{lines}"
        );

        config.plugins.registry_url = None;
        let lines = config_lines(&path, &root, &config).join("\n");
        assert!(lines.contains("registry_url = 未配置"), "{lines}");

        // 模板默认写官方商店，用户可替换或删掉。
        assert!(
            CONFIG_TEMPLATE.contains(
                "registry_url = \"https://catmoecircle.github.io/FuyuBot-plugins/index.json\""
            ),
            "模板缺默认商店地址：\n{CONFIG_TEMPLATE}"
        );
    }
}
