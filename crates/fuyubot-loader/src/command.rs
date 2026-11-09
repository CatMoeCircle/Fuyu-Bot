//! 命令闸门：用户账户下禁用 `/`，改用可配置的命令头，并限制谁能触发。
//!
//! `[command].prefix` 是**数组**，可以配多个命令头（如 `[".", "!", "。"]`）。
//! 没配时按账号类型取默认值：bot 认 `/`，user 认 `.`。配了就是**整套替换**——
//! bot 不会额外保留 `/`，要用就把它写进数组。
//!
//! 闸门在管线的 PreUpdate 之前跑，但**只决定通道**：被拦下的命令仍然会走完中间件、
//! 仍然会发到 `update` 通用通道（raw 监听器照常能看见），只是不进入
//! `updateNewMessage` 通道——`on_command` 因此不会响应它。
//!
//! 放行的非 `/` 命令头会被归一化成 `/cmd`，并把 `is_outgoing` 抹成 `false`；
//! 这样 `fuyubot-simplify::command_of` 与既有插件不用改一行就认得 `.cmd` / `!cmd`。
//! user 账户下 `/xxx` 始终不交给插件（Telegram 客户端自己会用 `/`），即使写进数组。
//!
//! 账号类型优先看**活的** `getMe`（`self_user.type.@type == "userTypeBot"`）；
//! 还没登录 / 回放时退回 `[telegram].account_type`（Android 写的是 `[telegram].account`），
//! 都没有就按 bot 处理——这是引入闸门之前的行为。

use std::collections::HashSet;

use serde_json::{Map, Value};

/// bot 账户没配 `[command].prefix` 时的默认命令头。
pub(crate) const DEFAULT_BOT_PREFIXES: [char; 1] = ['/'];

/// user 账户没配 `[command].prefix` 时的默认命令头。
pub(crate) const DEFAULT_USER_PREFIXES: [char; 1] = ['.'];

/// `[command]` 段 + `[telegram]` 账号类型解析出来的策略。
#[derive(Debug, Clone)]
pub(crate) struct CommandPolicy {
    /// `[command].prefix` 配置的命令头（数组）。`None` = 没配，按账号类型取
    /// [`DEFAULT_BOT_PREFIXES`] / [`DEFAULT_USER_PREFIXES`]。
    pub prefixes: Option<Vec<char>>,
    /// 允许触发命令的用户 id。自己的消息在用户账户下始终算数。
    pub whitelist: HashSet<i64>,
    /// user 账户的白名单模式：只响应自己与 `whitelist` 里的用户。
    ///
    /// 默认 `true`；`[command].whitelist_only = false` 可关（关掉后任何人都能触发），
    /// 但**不建议**——所以模板与文档里的注释必须写明。bot 账户不受它影响。
    pub whitelist_only: bool,
    /// 身份还没就绪时的兜底：`true` = bot（旧行为）。
    pub fallback_is_bot: bool,
}

impl Default for CommandPolicy {
    fn default() -> Self {
        Self {
            prefixes: None,
            whitelist: HashSet::new(),
            whitelist_only: true,
            fallback_is_bot: true,
        }
    }
}

impl CommandPolicy {
    /// 这个账号类型实际生效的命令头。配过就用配置的数组（即使里面没有 `/`），
    /// 没配才落到账号默认。
    pub(crate) fn prefixes_for(&self, account: AccountKind) -> &[char] {
        match &self.prefixes {
            Some(prefixes) => prefixes,
            None => match account {
                AccountKind::Bot => &DEFAULT_BOT_PREFIXES,
                AccountKind::User => &DEFAULT_USER_PREFIXES,
            },
        }
    }
}

/// 账号类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountKind {
    Bot,
    User,
}

/// 一条**放行的命令**的上下文：命令回复接口据此决定「发新消息」还是「编辑原消息」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandMeta {
    pub chat_id: i64,
    pub message_id: i64,
    /// 是不是用户账户**自己**发的：是的话，回复应当编辑这条命令消息。
    pub self_triggered: bool,
    /// 命令消息的 `ephemeral_message_id`（i32）；普通消息是 `None`。
    pub ephemeral_message_id: Option<i32>,
    /// 触发者用户 id（`sender_id.user_id`）；ephemeral 回复的 `receiver_user_id`。
    pub receiver_user_id: Option<i64>,
}

/// 闸门对一条更新的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// 不是命令，两条通道都发。
    Pass,
    /// 是一条放行的命令；上下文登记进宿主，供命令回复接口使用。
    Command(CommandMeta),
    /// 是命令，但不该触发：只发 `update` 通用通道，不发 `updateNewMessage`。
    Blocked,
}

impl CommandPolicy {
    /// 从完整配置树解析。返回策略与需要上报的告警——写错键只回退默认值，不让宿主起不来。
    pub(crate) fn from_config(root: &Value) -> (Self, Vec<String>) {
        let mut policy = Self::default();
        let mut warnings = Vec::new();
        let section = root.get("command");

        if let Some(raw) = section.and_then(|section| section.get("prefix")) {
            match parse_prefixes(raw) {
                Ok((prefixes, mut problems)) => {
                    warnings.append(&mut problems);
                    if prefixes.is_empty() {
                        warnings.push(fallback_warning());
                    } else {
                        policy.prefixes = Some(prefixes);
                    }
                }
                Err(problem) => {
                    warnings.push(format!("[command].prefix {problem}，{}", fallback_hint()))
                }
            }
        }

        if let Some(raw) = section.and_then(|section| section.get("whitelist")) {
            match parse_whitelist(raw) {
                Ok((list, mut problems)) => {
                    policy.whitelist = list;
                    warnings.append(&mut problems);
                }
                Err(problem) => {
                    warnings.push(format!("[command].whitelist {problem}，已按空表处理"));
                }
            }
        }

        if let Some(raw) = section.and_then(|section| section.get("whitelist_only")) {
            match raw.as_bool() {
                Some(value) => policy.whitelist_only = value,
                None => warnings.push(
                    "[command].whitelist_only 必须是 true / false，已按默认 true（白名单模式）处理"
                        .to_owned(),
                ),
            }
        }

        policy.fallback_is_bot = account_from_config(root).unwrap_or(true);
        (policy, warnings)
    }
}

/// 回退到账号默认值时统一的那半句告警。
fn fallback_hint() -> String {
    format!(
        "已回退到按账号默认（bot `{}`、user `{}`）",
        DEFAULT_BOT_PREFIXES[0], DEFAULT_USER_PREFIXES[0]
    )
}

/// 数组里一个能用的命令头都没有。
fn fallback_warning() -> String {
    format!("[command].prefix 没有可用的命令头，{}", fallback_hint())
}

/// `[command].prefix` 必须是**数组**，每一项是单个字符。
///
/// 单个字符的约束是「不是字母 / 数字 / 空白」：`/`、`.`、`!`、`。` 都行，
/// 这样中文命令头（`。help`）也能用。
fn parse_prefixes(value: &Value) -> Result<(Vec<char>, Vec<String>), &'static str> {
    let Some(items) = value.as_array() else {
        return Err("必须是数组，例如 [\".\", \"!\"]");
    };
    let mut prefixes = Vec::new();
    let mut warnings = Vec::new();
    for item in items {
        let Some(raw) = item.as_str() else {
            warnings.push(format!("[command].prefix 里的 {item} 不是字符串，已跳过"));
            continue;
        };
        let Some(prefix) = parse_prefix(raw) else {
            warnings.push(format!(
                "[command].prefix 里的 \"{raw}\" 不是单个命令头字符（必须恰好一个字符，且不能是字母、数字或空白），已跳过"
            ));
            continue;
        };
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    Ok((prefixes, warnings))
}

/// 单个命令头：恰好一个字符，且不是字母 / 数字 / 空白。
fn parse_prefix(raw: &str) -> Option<char> {
    let mut chars = raw.chars();
    let prefix = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    (!prefix.is_alphanumeric() && !prefix.is_whitespace()).then_some(prefix)
}

/// 白名单接受数字，也接受带引号的数字（手写配置里两种都常见）。
fn parse_whitelist(value: &Value) -> Result<(HashSet<i64>, Vec<String>), &'static str> {
    let Some(items) = value.as_array() else {
        return Err("必须是数组");
    };
    let mut set = HashSet::new();
    let mut warnings = Vec::new();
    for item in items {
        let id = item
            .as_i64()
            .or_else(|| item.as_str().and_then(|raw| raw.trim().parse().ok()));
        match id {
            Some(id) => {
                set.insert(id);
            }
            None => warnings.push(format!(
                "[command].whitelist 里的 {item} 不是用户 id，已跳过"
            )),
        }
    }
    Ok((set, warnings))
}

/// `[telegram].account_type`（CLI）或 `[telegram].account`（Android）。
fn account_from_config(root: &Value) -> Option<bool> {
    let telegram = root.get("telegram")?;
    ["account_type", "account"].iter().find_map(|key| {
        let raw = telegram.get(*key)?.as_str()?.trim();
        match raw.to_ascii_lowercase().as_str() {
            "bot" => Some(true),
            "user" => Some(false),
            _ => None,
        }
    })
}

/// 消息文本（只认 `messageText`，与 `command_of` 同一条字段路径）。
fn message_text(message: &Map<String, Value>) -> Option<&str> {
    let content = message.get("content")?;
    if content.get("@type")?.as_str()? != "messageText" {
        return None;
    }
    content.get("text")?.get("text")?.as_str()
}

/// 发送者是不是一个用户；返回用户 id。
fn sender_user_id(message: &Map<String, Value>) -> Option<i64> {
    let sender = message.get("sender_id")?;
    if sender.get("@type")?.as_str()? != "messageSenderUser" {
        return None;
    }
    sender.get("user_id")?.as_i64()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixKind {
    Slash,
    Custom,
}

/// 文本开头（忽略前导空白）是不是一条命令。
///
/// 返回 `(命令类型, 前缀起点字节偏移, 前缀字节长度)`。`"/"` / `"."` 这种
/// 只有前缀没有名字的不算命令——与 `command_of` 的 `name.is_empty()` 对齐。
fn command_at(text: &str, prefixes: &[char]) -> Option<(PrefixKind, usize, usize)> {
    let start = text.len() - text.trim_start().len();
    let rest = &text[start..];
    let token = rest.split_whitespace().next()?;
    let first = token.chars().next()?;
    if !prefixes.contains(&first) {
        return None;
    }
    let kind = if first == '/' {
        PrefixKind::Slash
    } else {
        PrefixKind::Custom
    };
    let after = &token[first.len_utf8()..];
    if after.is_empty() {
        return None;
    }
    Some((kind, start, first.len_utf8()))
}

/// 这段文本看起来是不是命令（与闸门同一套判定）。命令回复接口用它决定要不要记指纹。
pub(crate) fn looks_like_command(text: &str, prefixes: &[char]) -> bool {
    command_at(text, prefixes).is_some()
}

/// 对一条更新执行闸门。只可能处理 `updateNewMessage`，其它类型原样放行。
///
/// `host_reply` 用来识别「宿主通过命令回复接口发出的消息」：命中就当作命令回复，
/// 不进 `updateNewMessage` 通道（防止自己的回复再触发命令）。只在消息是用户自己发的时调用。
pub(crate) fn gate(
    payload: &mut Value,
    policy: &CommandPolicy,
    account: AccountKind,
    self_user_id: Option<i64>,
    host_reply: &mut dyn FnMut(i64, &str) -> bool,
) -> Gate {
    if payload.get("@type").and_then(Value::as_str) != Some("updateNewMessage") {
        return Gate::Pass;
    }
    let Some(message) = payload.get_mut("message").and_then(Value::as_object_mut) else {
        return Gate::Pass;
    };
    let Some(text) = message_text(message) else {
        return Gate::Pass;
    };
    let prefixes = policy.prefixes_for(account);
    // user 账户下 `/` 永远算命令（然后被拦下），即使数组里没写它——
    // 这样 `/xxx` 不会作为普通消息漏进 `updateNewMessage` 通道。
    let found = command_at(text, prefixes).or_else(|| {
        if account == AccountKind::User {
            command_at(text, &['/'])
        } else {
            None
        }
    });
    let Some((kind, start, prefix_len)) = found else {
        return Gate::Pass;
    };
    let (Some(chat_id), Some(message_id)) = (
        message.get("chat_id").and_then(Value::as_i64),
        message.get("id").and_then(Value::as_i64),
    ) else {
        return Gate::Pass;
    };

    let is_outgoing = message
        .get("is_outgoing")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sender = sender_user_id(message);
    let ephemeral_message_id = message
        .get("ephemeral_message_id")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id > 0);
    let self_triggered = account == AccountKind::User
        && (is_outgoing || sender.is_some_and(|id| Some(id) == self_user_id));
    // 宿主自己发的命令回复：不进入命令通道（`update` 通用通道仍然发）。
    if self_triggered && host_reply(chat_id, text) {
        return Gate::Blocked;
    }

    let allowed = match account {
        AccountKind::Bot => {
            // bot 账户下 `/` 与配置的自定义命令头一视同仁：自己的出站消息不触发，
            // 白名单非空时只认表里的用户。自定义头放行后会被归一化成 `/cmd`。
            !is_outgoing
                && (policy.whitelist.is_empty()
                    || sender.is_some_and(|id| policy.whitelist.contains(&id)))
        }
        AccountKind::User => match kind {
            // 用户账户下 `/xxx` 一律不交给插件——自己发的也不认。
            PrefixKind::Slash => false,
            // 自定义命令头：白名单模式只认自己 / 表里的用户；关掉后任何人都能触发
            // （`whitelist_only = false`，不建议）。
            PrefixKind::Custom => {
                !policy.whitelist_only
                    || self_triggered
                    || sender.is_some_and(|id| policy.whitelist.contains(&id))
            }
        },
    };

    if !allowed {
        return Gate::Blocked;
    }
    if kind == PrefixKind::Custom {
        rewrite(message, start, prefix_len, is_outgoing);
    }
    Gate::Command(CommandMeta {
        chat_id,
        message_id,
        self_triggered,
        ephemeral_message_id,
        receiver_user_id: sender,
    })
}

/// 把 `.cmd` 就地改写成 `/cmd`，并把出站标记抹平，让既有命令解析器认得。
fn rewrite(message: &mut Map<String, Value>, start: usize, prefix_len: usize, is_outgoing: bool) {
    let Some(text) = message
        .get_mut("content")
        .and_then(|content| content.get_mut("text"))
        .and_then(|text| text.get_mut("text"))
    else {
        return;
    };
    let Some(original) = text.as_str() else {
        return;
    };
    if start + prefix_len > original.len() {
        return;
    }
    let mut rewritten = String::with_capacity(original.len());
    rewritten.push_str(&original[..start]);
    rewritten.push('/');
    rewritten.push_str(&original[start + prefix_len..]);
    *text = Value::String(rewritten);
    if is_outgoing {
        message.insert("is_outgoing".to_owned(), Value::Bool(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(text: &str, outgoing: bool, sender: i64) -> Value {
        json!({
            "@type": "updateNewMessage",
            "message": {
                "id": 1,
                "chat_id": sender,
                "is_outgoing": outgoing,
                "sender_id": {"@type": "messageSenderUser", "user_id": sender},
                "content": {
                    "@type": "messageText",
                    "text": {"@type": "formattedText", "text": text}
                }
            }
        })
    }

    fn policy(prefixes: &[char], whitelist: &[i64]) -> CommandPolicy {
        CommandPolicy {
            prefixes: Some(prefixes.to_vec()),
            whitelist: whitelist.iter().copied().collect(),
            whitelist_only: true,
            fallback_is_bot: true,
        }
    }

    /// 没配 `[command].prefix`：命令头按账号类型取默认值。
    fn defaults(whitelist: &[i64]) -> CommandPolicy {
        CommandPolicy {
            whitelist: whitelist.iter().copied().collect(),
            ..CommandPolicy::default()
        }
    }

    fn text_of(payload: &Value) -> &str {
        payload["message"]["content"]["text"]["text"]
            .as_str()
            .unwrap()
    }

    fn run(
        payload: &mut Value,
        policy: &CommandPolicy,
        account: AccountKind,
        self_id: Option<i64>,
    ) -> Gate {
        gate(payload, policy, account, self_id, &mut |_, _| false)
    }

    fn command_meta(gate: Gate) -> CommandMeta {
        match gate {
            Gate::Command(meta) => meta,
            other => panic!("期望 Command，实际 {other:?}"),
        }
    }

    #[test]
    fn bot_keeps_slash_commands_from_anyone() {
        let policy = defaults(&[]);
        let mut payload = update("/start", false, 9);
        let meta = command_meta(run(&mut payload, &policy, AccountKind::Bot, Some(1)));
        assert!(!meta.self_triggered);
        assert_eq!(text_of(&payload), "/start");
    }

    #[test]
    fn bot_drops_its_own_outgoing_commands() {
        let policy = defaults(&[]);
        let mut payload = update("/start", true, 1);
        assert_eq!(
            run(&mut payload, &policy, AccountKind::Bot, Some(1)),
            Gate::Blocked
        );
    }

    #[test]
    fn bot_whitelist_filters_commands() {
        let policy = defaults(&[7]);
        let mut allowed = update("/start", false, 7);
        assert!(
            matches!(
                run(&mut allowed, &policy, AccountKind::Bot, Some(1)),
                Gate::Command(_)
            ),
            "白名单里的用户应该放行"
        );
        let mut denied = update("/start", false, 9);
        assert_eq!(
            run(&mut denied, &policy, AccountKind::Bot, Some(1)),
            Gate::Blocked
        );
    }

    /// 没配 `[command].prefix` 时 bot 只认 `/`：`.` 原样当普通消息。
    #[test]
    fn bot_defaults_to_slash_only() {
        let policy = defaults(&[]);
        let mut payload = update(".start", false, 9);
        assert_eq!(
            run(&mut payload, &policy, AccountKind::Bot, Some(1)),
            Gate::Pass
        );
        assert_eq!(text_of(&payload), ".start", "bot 不该改写文本");
    }

    #[test]
    fn user_drops_all_slash_commands_even_from_self() {
        let policy = policy(&['.'], &[9]);
        for (outgoing, sender) in [(true, 7), (false, 7), (false, 9)] {
            let mut payload = update("/start", outgoing, sender);
            assert_eq!(
                run(&mut payload, &policy, AccountKind::User, Some(7)),
                Gate::Blocked,
                "outgoing={outgoing} sender={sender}"
            );
        }
    }

    #[test]
    fn user_allows_own_dot_command_and_normalizes_it() {
        let policy = policy(&['.'], &[]);
        let mut payload = update(".start args", true, 7);
        let meta = command_meta(run(&mut payload, &policy, AccountKind::User, Some(7)));
        assert!(meta.self_triggered);
        assert_eq!(meta.chat_id, 7);
        assert_eq!(meta.message_id, 1);
        assert_eq!(text_of(&payload), "/start args");
        assert_eq!(payload["message"]["is_outgoing"], json!(false));
    }

    #[test]
    fn user_drops_other_peoples_dot_commands() {
        let policy = policy(&['.'], &[]);
        let mut payload = update(".start", false, 9);
        assert_eq!(
            run(&mut payload, &policy, AccountKind::User, Some(7)),
            Gate::Blocked
        );
    }

    #[test]
    fn user_allows_whitelisted_senders() {
        let policy = policy(&['.'], &[9]);
        let mut payload = update(".start", false, 9);
        let meta = command_meta(run(&mut payload, &policy, AccountKind::User, Some(7)));
        assert!(!meta.self_triggered, "白名单里的别人触发，回复要发新消息");
        assert_eq!(text_of(&payload), "/start");
    }

    #[test]
    fn user_accepts_self_by_id_when_outgoing_flag_is_missing() {
        let policy = policy(&['.'], &[]);
        let mut payload = update(".start", false, 7);
        let meta = command_meta(run(&mut payload, &policy, AccountKind::User, Some(7)));
        assert!(meta.self_triggered);
    }

    /// 白名单模式默认开着；`whitelist_only = false` 时任何人的命令都放行（不建议）。
    #[test]
    fn user_whitelist_mode_can_be_turned_off() {
        let mut policy = policy(&['.'], &[]);
        policy.whitelist_only = false;

        let mut stranger = update(".start", false, 9);
        let meta = command_meta(run(&mut stranger, &policy, AccountKind::User, Some(7)));
        assert!(!meta.self_triggered, "别人触发，回复要发新消息");
        assert_eq!(text_of(&stranger), "/start");

        // 自己发的仍然算自己触发（回复走编辑），不受开关影响。
        let mut own = update(".start", true, 7);
        let meta = command_meta(run(&mut own, &policy, AccountKind::User, Some(7)));
        assert!(meta.self_triggered);
        assert_eq!(text_of(&own), "/start");
    }

    /// 开关只作用于 user 账户：bot 的 whitelist 规则保持原样。
    #[test]
    fn user_whitelist_mode_does_not_change_bot() {
        let mut policy = policy(&[], &[7]);
        policy.prefixes = None;
        policy.whitelist_only = false;

        let mut listed = update("/start", false, 7);
        assert!(matches!(
            run(&mut listed, &policy, AccountKind::Bot, Some(1)),
            Gate::Command(_)
        ));
        let mut stranger = update("/start", false, 9);
        assert_eq!(
            run(&mut stranger, &policy, AccountKind::Bot, Some(1)),
            Gate::Blocked,
            "bot 仍按 whitelist 过滤"
        );
    }

    #[test]
    fn leading_whitespace_is_kept_when_rewriting() {
        let policy = policy(&['.'], &[]);
        let mut payload = update("  .start", true, 7);
        assert!(matches!(
            run(&mut payload, &policy, AccountKind::User, Some(7)),
            Gate::Command(_)
        ));
        assert_eq!(text_of(&payload), "  /start");
    }

    #[test]
    fn the_prefix_is_configurable() {
        let policy = policy(&['!'], &[]);
        let mut payload = update("!help", true, 7);
        assert!(matches!(
            run(&mut payload, &policy, AccountKind::User, Some(7)),
            Gate::Command(_)
        ));
        assert_eq!(text_of(&payload), "/help");
    }

    /// 数组里的每个命令头都认，包括中文标点；放行后统一归一化成 `/`。
    #[test]
    fn every_configured_prefix_is_accepted_and_normalized() {
        let policy = policy(&['.', '!', '。'], &[]);
        for (text, rewritten) in [
            (".help", "/help"),
            ("!help", "/help"),
            ("。help", "/help"),
            ("。帮助 参数", "/帮助 参数"),
        ] {
            let mut payload = update(text, true, 7);
            assert!(
                matches!(
                    run(&mut payload, &policy, AccountKind::User, Some(7)),
                    Gate::Command(_)
                ),
                "{text}"
            );
            assert_eq!(text_of(&payload), rewritten, "{text}");
        }
    }

    /// 配了数组就是**整套替换**：bot 也只用数组里的头，没写 `/` 就不认 `/`。
    #[test]
    fn configured_prefixes_replace_the_bot_default() {
        let policy = policy(&['.', '!'], &[]);
        let mut custom = update(".start", false, 9);
        let meta = command_meta(run(&mut custom, &policy, AccountKind::Bot, Some(1)));
        assert!(!meta.self_triggered);
        assert_eq!(text_of(&custom), "/start", "bot 的自定义头也要归一化");

        let mut slash = update("/start", false, 9);
        assert_eq!(
            run(&mut slash, &policy, AccountKind::Bot, Some(1)),
            Gate::Pass,
            "数组里没有 `/`，bot 不该再认它"
        );
        assert_eq!(text_of(&slash), "/start", "放行普通消息不改写");
    }

    /// user 账户下 `/` 即使写进数组也不交给插件（Telegram 客户端占用 `/`）。
    #[test]
    fn a_slash_in_the_user_array_is_still_blocked() {
        let policy = policy(&['/', '.'], &[]);
        let mut slash = update("/start", true, 7);
        assert_eq!(
            run(&mut slash, &policy, AccountKind::User, Some(7)),
            Gate::Blocked
        );

        let mut dot = update(".start", true, 7);
        assert!(matches!(
            run(&mut dot, &policy, AccountKind::User, Some(7)),
            Gate::Command(_)
        ));
    }

    #[test]
    fn a_bare_prefix_is_not_a_command() {
        let policy = policy(&['.'], &[]);
        for text in [".", ". help", "plain text"] {
            let mut payload = update(text, true, 7);
            assert_eq!(
                run(&mut payload, &policy, AccountKind::User, Some(7)),
                Gate::Pass,
                "{text}"
            );
            assert_eq!(text_of(&payload), text);
        }
    }

    /// 宿主自己发出的命令回复：不进命令通道，而且**不能改写**原文（raw 监听器还要看原样）。
    #[test]
    fn an_outgoing_host_reply_command_is_blocked_without_rewriting() {
        let policy = policy(&['.'], &[]);
        let mut payload = update(".help", true, 7);
        let mut seen = None;
        let decision = gate(
            &mut payload,
            &policy,
            AccountKind::User,
            Some(7),
            &mut |chat_id, text| {
                seen = Some((chat_id, text.to_owned()));
                true
            },
        );
        assert_eq!(decision, Gate::Blocked);
        assert_eq!(seen, Some((7, ".help".to_owned())));
        assert_eq!(text_of(&payload), ".help", "被拦下时不能改写");
        assert_eq!(payload["message"]["is_outgoing"], json!(true));

        // 入站消息不查这个指纹：别人发同样的文本不该被当成宿主回复。
        let mut incoming = update(".help", false, 8);
        let decision = gate(
            &mut incoming,
            &policy,
            AccountKind::User,
            Some(7),
            &mut |_, _| panic!("入站不该查宿主回复指纹"),
        );
        assert_eq!(decision, Gate::Blocked, "不在白名单的别人照样拦");
    }

    /// ephemeral 命令要把 id 和发送者带进上下文，回复才能走 sendEphemeralMessage。
    #[test]
    fn ephemeral_commands_carry_id_and_sender() {
        let policy = defaults(&[]);
        let mut payload = update("/help", false, 9);
        payload["message"]["ephemeral_message_id"] = json!(555);
        let meta = command_meta(run(&mut payload, &policy, AccountKind::Bot, Some(1)));
        assert_eq!(meta.ephemeral_message_id, Some(555));
        assert_eq!(meta.receiver_user_id, Some(9));
    }

    #[test]
    fn non_text_and_other_updates_pass_untouched() {
        let policy = policy(&['.'], &[]);
        let mut other = json!({"@type": "updateDeleteMessages", "message_ids": [1]});
        assert_eq!(
            run(&mut other, &policy, AccountKind::User, Some(7)),
            Gate::Pass
        );

        let mut photo = update(".start", true, 7);
        photo["message"]["content"] = json!({"@type": "messagePhoto"});
        assert_eq!(
            run(&mut photo, &policy, AccountKind::User, Some(7)),
            Gate::Pass
        );
    }

    #[test]
    fn config_parses_prefix_array_and_whitelist() {
        let (policy, warnings) = CommandPolicy::from_config(&json!({
            "telegram": {"account_type": "user"},
            "command": {"prefix": [".", "!", "。"], "whitelist": [7, "8"]}
        }));
        assert_eq!(policy.prefixes, Some(vec!['.', '!', '。']));
        assert_eq!(policy.prefixes_for(AccountKind::User), &['.', '!', '。']);
        assert!(policy.whitelist.contains(&7));
        assert!(policy.whitelist.contains(&8));
        assert!(policy.whitelist_only, "user 账户默认白名单模式");
        assert!(!policy.fallback_is_bot);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn config_can_turn_off_the_user_whitelist_mode() {
        let (policy, warnings) = CommandPolicy::from_config(&json!({
            "command": {"whitelist_only": false}
        }));
        assert!(!policy.whitelist_only);
        assert!(warnings.is_empty(), "{warnings:?}");

        // 写错类型：按默认 true 处理并告警。
        let (policy, warnings) = CommandPolicy::from_config(&json!({
            "command": {"whitelist_only": "false"}
        }));
        assert!(policy.whitelist_only);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("whitelist_only"), "{warnings:?}");
    }

    /// `prefix` 只接受数组：旧写法（单个字符串）告警并回退默认值。
    #[test]
    fn config_requires_an_array() {
        for raw in [json!("."), json!("!"), json!(7), json!(true)] {
            let (policy, warnings) = CommandPolicy::from_config(&json!({
                "command": {"prefix": raw}
            }));
            assert_eq!(policy.prefixes, None);
            assert_eq!(warnings.len(), 1, "{raw}");
            assert!(warnings[0].contains("必须是数组"), "{warnings:?}");
        }
    }

    #[test]
    fn config_skips_bad_prefix_entries_and_keeps_the_good_ones() {
        let (policy, warnings) = CommandPolicy::from_config(&json!({
            "command": {"prefix": [".", "", "ab", "a", " ", 7, "!", "."]}
        }));
        assert_eq!(policy.prefixes, Some(vec!['.', '!']));
        assert_eq!(warnings.len(), 5, "{warnings:?}");
    }

    #[test]
    fn an_empty_or_all_invalid_prefix_array_falls_back_to_account_defaults() {
        for raw in [json!([]), json!(["", "ab", 7])] {
            let (policy, warnings) = CommandPolicy::from_config(&json!({
                "command": {"prefix": raw}
            }));
            assert_eq!(policy.prefixes, None, "{raw}");
            assert!(!warnings.is_empty(), "{raw}");
            assert_eq!(policy.prefixes_for(AccountKind::Bot), &['/']);
            assert_eq!(policy.prefixes_for(AccountKind::User), &['.']);
        }
    }

    #[test]
    fn config_allows_slash_in_the_array() {
        let (policy, warnings) = CommandPolicy::from_config(&json!({
            "command": {"prefix": ["/", "."]}
        }));
        assert_eq!(policy.prefixes, Some(vec!['/', '.']));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn config_accepts_the_android_account_key() {
        let (policy, _) = CommandPolicy::from_config(&json!({
            "telegram": {"account": "user"}
        }));
        assert!(!policy.fallback_is_bot);
    }

    #[test]
    fn config_skips_bad_whitelist_entries() {
        let (policy, warnings) = CommandPolicy::from_config(&json!({
            "command": {"whitelist": [7, "x", true]}
        }));
        assert_eq!(policy.whitelist, HashSet::from([7]));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
