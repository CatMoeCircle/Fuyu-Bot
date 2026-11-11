//! 内置插件：`/help` 从**宿主**的命令能力表渲染。
//!
//! 命令表不在这个插件里：插件在 `start()` 里经宿主预置的
//! `fuyubot.host.commands` service 登记，宿主按 owner 管生命周期（卸载即回滚）。
//! `/help` 只做两件事：
//!
//! 1. 问宿主要 `list`（命令名、说明、归属插件、角色、允许的会话）；
//! 2. 按「全局角色 × 会话类型」过滤后按插件分组渲染。
//!
//! 过滤必须在**回复线程**里做：群管理员要问 `getChatMember`，不能在接收循环上等
//! TDLib。`/help` 的回调因此起一个工作线程，先解析权限，再 `reply`。
//!
//! `chat_admin` 是**会话角色**：群里的 Telegram 管理员不等于 `[roles].admins`。
//! 频道里发帖的本来就是频道身份，所以频道按「频道管理员」处理。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use fuyubot_abi::{HostApi, LogLevel, Plugin, define_plugin};
use fuyubot_simplify::{
    Command, CommandSubscription, SendMessageParams, Simplify, Text, on_command,
};
use serde_json::{Value, json};

/// 宿主命令能力表 service。
const HOST_COMMANDS: &str = "fuyubot.host.commands";

/// TDLib 往返的超时（`getChat` / `getChatMember`）。
const LOOKUP_TIMEOUT_MS: u64 = 1_500;

/// 会话类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatKind {
    Private,
    Group,
    Channel,
}

/// 命令要求的最低角色（命令表里 `role` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    ChatAdmin,
    Admin,
    Owner,
}

impl Role {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "user" => Some(Self::User),
            "chat_admin" | "chat-admin" | "chatadmin" => Some(Self::ChatAdmin),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }

    /// 这个人能不能看到 / 使用要求该角色的命令。
    fn allows(self, global: GlobalRole, chat_admin: bool) -> bool {
        match self {
            Self::User => true,
            Self::ChatAdmin => chat_admin,
            Self::Admin => matches!(global, GlobalRole::Admin | GlobalRole::Owner),
            Self::Owner => global == GlobalRole::Owner,
        }
    }
}

/// 配置算出的全局角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalRole {
    User,
    Admin,
    Owner,
}

impl GlobalRole {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            _ => Self::User,
        }
    }
}

/// 命令表里的一条（`/help` 需要的字段）。
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    description: String,
    plugin: Option<String>,
    role: Role,
    scopes: Vec<ChatKind>,
    hidden: bool,
}

impl Entry {
    fn visible(&self, global: GlobalRole, chat_admin: bool, kind: ChatKind) -> bool {
        !self.hidden && self.role.allows(global, chat_admin) && self.scopes.contains(&kind)
    }
}

struct HelpPlugin {
    host: HostApi,
    subscription: Option<CommandSubscription>,
    /// 还在跑的异步回复（`stop()` 里 join 掉：卸载之后不能再跑库里的代码）。
    sending: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Plugin for HelpPlugin {
    fn init(host: HostApi) -> Result<Self, String> {
        Ok(Self {
            host,
            subscription: None,
            sending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn start(&mut self, _config_json: &str) -> Result<(), String> {
        let host = self.host;

        // `/help` 自己也要在表里：它现在是普通命令，不是特例。
        register_help(host);

        let sending = Arc::clone(&self.sending);
        let subscription = on_command(host, move |command: &Command| {
            if !command.is("help") {
                return;
            }
            let host = host;
            let sending = Arc::clone(&sending);
            let command = command.clone();
            // 回调在接收循环上：权限查询可能等 TDLib，必须挪到工作线程。
            let handle = std::thread::spawn(move || {
                let params = render_params(host, &command);
                if let Err(error) = command.reply(host, &params) {
                    host.log(LogLevel::Error, &format!("help 插件回复失败: {error}"));
                }
            });
            if let Ok(mut sending) = sending.lock() {
                sending.push(handle);
            }
        });
        let subscription = subscription.ok_or_else(|| "订阅 updateNewMessage 失败".to_owned())?;
        self.subscription = Some(subscription);
        Ok(())
    }

    fn stop(&mut self) {
        self.subscription = None;
        let mut sending = self
            .sending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for handle in sending.drain(..) {
            let _ = handle.join();
        }
    }

    /// `/plugins` 列表里显示的名字。
    fn display_name() -> &'static str {
        "帮助"
    }

    /// `/plugins` 列表里那一行介绍。
    fn description() -> &'static str {
        "汇总并回复 /help 的命令表"
    }
}

/// 把 `/help` 自己登记进宿主命令表。失败只留一条日志：`/help` 本身仍然能用，
/// 只是它不出现在自己的列表里。
fn register_help(host: HostApi) {
    let input = json!({
        "name": "help",
        "description": "这条帮助",
        "role": "user",
        "scopes": ["private", "group", "channel"],
    });
    match host.call_service(HOST_COMMANDS, "register", &input.to_string()) {
        Ok(_) => {}
        Err(error) => host.log(LogLevel::Warn, &format!("help 插件登记到宿主失败: {error}")),
    }
}

/// 在工作线程里算出回复参数。
fn render_params(host: HostApi, command: &Command) -> SendMessageParams {
    let global = global_role(host, command.sender_user_id);
    let kind = chat_kind(host, command.chat_id);
    let chat_admin = match kind {
        ChatKind::Private => false,
        // 能在频道里发帖的本来就是频道管理员（TDLib 的发送者是 messageSenderChat）。
        ChatKind::Channel => true,
        ChatKind::Group => command
            .sender_user_id
            .is_some_and(|user_id| is_chat_admin(host, command.chat_id, user_id)),
    };
    let entries = list_entries(host);
    let text = render_help(
        &entries,
        &plugin_display_names(host),
        global,
        kind,
        chat_admin,
    );
    SendMessageParams {
        text: Some(Text::markdown(text)),
        link_preview: Some(false),
        ..SendMessageParams::default()
    }
}

/// 问宿主「这个人的全局角色」：纯内存，不碰 TDLib。
fn global_role(host: HostApi, user_id: Option<i64>) -> GlobalRole {
    let Some(user_id) = user_id else {
        return GlobalRole::User;
    };
    let input = json!({ "user_id": user_id });
    let Ok(raw) = host.call_service(HOST_COMMANDS, "role", &input.to_string()) else {
        return GlobalRole::User;
    };
    let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    value
        .get("global")
        .and_then(Value::as_str)
        .map(GlobalRole::parse)
        .unwrap_or(GlobalRole::User)
}

/// 会话类型：私聊看 id 符号，群 / 频道要问 `getChat`（两者 id 都是负的）。
fn chat_kind(host: HostApi, chat_id: i64) -> ChatKind {
    if chat_id > 0 {
        return ChatKind::Private;
    }
    // 会话类型 / 管理权的短缓存都在宿主（`fuyubot.host.cache`），这里不再存第二份：
    // 否则 help 自己的 60 秒会比宿主的 30 秒 TTL 更旧，权限判断反而更危险。
    Simplify::new(host)
        .chat(chat_id, LOOKUP_TIMEOUT_MS)
        .as_ref()
        .and_then(kind_of_chat)
        .unwrap_or(ChatKind::Group)
}

/// `getChat` 的 `type`：`chatTypePrivate` / `chatTypeBasicGroup` /
/// `chatTypeSupergroup`（再看 `is_channel`）。
fn kind_of_chat(chat: &Value) -> Option<ChatKind> {
    match chat.pointer("/type/@type")?.as_str()? {
        "chatTypePrivate" => Some(ChatKind::Private),
        "chatTypeBasicGroup" => Some(ChatKind::Group),
        "chatTypeSupergroup" => {
            let is_channel = chat
                .pointer("/type/is_channel")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(if is_channel {
                ChatKind::Channel
            } else {
                ChatKind::Group
            })
        }
        _ => None,
    }
}

/// 群管理权。查不到一律 `false`：不确定时说「没有」比说「有」安全。
///
/// `Simplify::chat_member` 走宿主缓存，`chat_member` 条目带 30 秒 TTL，
/// 所以这里不需要再套一层 help 自己的缓存。
fn is_chat_admin(host: HostApi, chat_id: i64, user_id: i64) -> bool {
    Simplify::new(host).is_chat_admin(chat_id, user_id, None, LOOKUP_TIMEOUT_MS)
}

/// 从宿主读整张命令表。读不到 / 形状不对就空表——`/help` 会回一句「没有可用命令」，
/// 而不是把插件搞崩。
fn list_entries(host: HostApi) -> Vec<Entry> {
    let Ok(raw) = host.call_service(HOST_COMMANDS, "list", "{}") else {
        return Vec::new();
    };
    let Ok(items) = serde_json::from_str::<Vec<Value>>(&raw) else {
        return Vec::new();
    };
    items.iter().filter_map(entry_of).collect()
}

fn entry_of(value: &Value) -> Option<Entry> {
    let name = value.get("name")?.as_str()?.to_owned();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let plugin = value
        .get("plugin")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let role = Role::parse(value.get("role").and_then(Value::as_str).unwrap_or("user"))?;
    let scopes = value
        .get("scopes")?
        .as_array()?
        .iter()
        .filter_map(|scope| scope.as_str())
        .filter_map(parse_scope)
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return None;
    }
    let hidden = value
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(Entry {
        name,
        description,
        plugin,
        role,
        scopes,
        hidden,
    })
}

fn parse_scope(raw: &str) -> Option<ChatKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "private" | "pm" | "dm" => Some(ChatKind::Private),
        "group" | "supergroup" => Some(ChatKind::Group),
        "channel" => Some(ChatKind::Channel),
        _ => None,
    }
}

/// 问宿主「插件名 → 展示名」。读不到（老宿主 / 没装插件目录）就空表，渲染退回短名。
///
/// 只读宿主报告，**不 dlopen**：名字只是用来排版，不值得为它加载任何东西。
fn plugin_display_names(host: HostApi) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    let Ok(report) = serde_json::from_str::<Value>(&host.list_plugins()) else {
        return names;
    };
    let Some(plugins) = report.get("plugins").and_then(Value::as_array) else {
        return names;
    };
    for plugin in plugins {
        let Some(name) = plugin.get("name").and_then(Value::as_str) else {
            continue;
        };
        let display = plugin
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !display.is_empty() {
            names.insert(name.to_owned(), display.to_owned());
        }
    }
    names
}

/// 过滤 + 按插件分组渲染。组按插件名排序（稳定输出），「其它」永远在最后。
fn render_help(
    entries: &[Entry],
    display_names: &BTreeMap<String, String>,
    global: GlobalRole,
    kind: ChatKind,
    chat_admin: bool,
) -> String {
    let mut groups: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    let mut unknown = Vec::new();
    for entry in entries {
        if !entry.visible(global, chat_admin, kind) {
            continue;
        }
        match entry.plugin.as_deref() {
            Some(plugin) => groups.entry(plugin).or_default().push(entry),
            None => unknown.push(entry),
        }
    }

    if groups.is_empty() && unknown.is_empty() {
        return "*可用命令*\n\n当前会话没有可用的命令。".to_owned();
    }

    let mut text = String::from("*可用命令*\n");
    for (plugin, commands) in groups {
        text.push('\n');
        text.push_str(&format!("*{}*\n", group_title(plugin, display_names)));
        for command in commands {
            push_command(&mut text, command);
        }
    }
    if !unknown.is_empty() {
        text.push_str("\n*其它*\n");
        for command in unknown {
            push_command(&mut text, command);
        }
    }
    text
}

/// 一行的格式：`/name — 说明`；没有说明就只写 `/name`（不留悬空的破折号）。
fn push_command(text: &mut String, command: &Entry) {
    if command.description.is_empty() {
        text.push_str(&format!("/{}\n", command.name));
    } else {
        text.push_str(&format!("/{} — {}\n", command.name, command.description));
    }
}

/// 组标题：展示名 + 短名（`启动问候（start）`）；没有展示名就只写短名。
///
/// 展示名与短名相同时不重复写两遍。
fn group_title(plugin: &str, display_names: &BTreeMap<String, String>) -> String {
    let short = plugin.rsplit('.').next().unwrap_or(plugin);
    match display_names.get(plugin) {
        Some(display) if display.as_str() != short => format!("{display}（{short}）"),
        _ => short.to_owned(),
    }
}

define_plugin!(HelpPlugin, "fuyubot.help");

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, role: Role, scopes: &[ChatKind], plugin: Option<&str>) -> Entry {
        Entry {
            name: name.to_owned(),
            description: "说明".to_owned(),
            plugin: plugin.map(str::to_owned),
            role,
            scopes: scopes.to_vec(),
            hidden: false,
        }
    }

    #[test]
    fn owner_sees_owner_commands_but_a_user_does_not() {
        let entries = vec![
            entry("start", Role::User, &[ChatKind::Private], None),
            entry("config", Role::Owner, &[ChatKind::Private], None),
        ];
        let names = BTreeMap::new();
        let as_user = render_help(&entries, &names, GlobalRole::User, ChatKind::Private, false);
        assert!(as_user.contains("/start"), "{as_user}");
        assert!(!as_user.contains("/config"), "{as_user}");

        let as_owner = render_help(
            &entries,
            &names,
            GlobalRole::Owner,
            ChatKind::Private,
            false,
        );
        assert!(as_owner.contains("/config"), "{as_owner}");
    }

    /// 群管理员只在本群看到 `chat_admin` 命令；全局管理员不会因此多看见它们。
    #[test]
    fn chat_admin_commands_are_scoped_to_the_group() {
        let entries = vec![entry(
            "ban",
            Role::ChatAdmin,
            &[ChatKind::Group],
            Some("fuyubot.mod"),
        )];
        let names = BTreeMap::from([("fuyubot.mod".to_owned(), "管理".to_owned())]);
        let as_member = render_help(&entries, &names, GlobalRole::User, ChatKind::Group, false);
        assert!(!as_member.contains("/ban"), "{as_member}");
        let as_admin = render_help(&entries, &names, GlobalRole::User, ChatKind::Group, true);
        assert!(as_admin.contains("/ban"), "{as_admin}");
        let as_owner_private = render_help(
            &entries,
            &names,
            GlobalRole::Owner,
            ChatKind::Private,
            false,
        );
        assert!(!as_owner_private.contains("/ban"), "{as_owner_private}");
    }

    #[test]
    fn channel_only_commands_stay_in_channels() {
        let entries = vec![entry("post", Role::User, &[ChatKind::Channel], None)];
        let names = BTreeMap::new();
        let channel = render_help(&entries, &names, GlobalRole::User, ChatKind::Channel, true);
        assert!(channel.contains("/post"), "{channel}");
        let group = render_help(&entries, &names, GlobalRole::User, ChatKind::Group, false);
        assert!(!group.contains("/post"), "{group}");
    }

    #[test]
    fn hidden_commands_are_not_rendered() {
        let mut hidden = entry("secret", Role::User, &[ChatKind::Private], None);
        hidden.hidden = true;
        let text = render_help(
            &[hidden],
            &BTreeMap::new(),
            GlobalRole::Owner,
            ChatKind::Private,
            false,
        );
        assert!(!text.contains("/secret"), "{text}");
        assert!(text.contains("没有可用"), "{text}");
    }

    #[test]
    fn parsing_a_host_entry_keeps_role_and_scopes() {
        let value = json!({
            "name": "ban",
            "description": "禁言",
            "plugin": "fuyubot.mod",
            "role": "chat_admin",
            "scopes": ["group"],
            "hidden": false
        });
        let entry = entry_of(&value).expect("合法条目");
        assert_eq!(entry.role, Role::ChatAdmin);
        assert_eq!(entry.scopes, vec![ChatKind::Group]);
        assert_eq!(entry.plugin.as_deref(), Some("fuyubot.mod"));
    }
}
