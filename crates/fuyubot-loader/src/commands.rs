//! 宿主侧的命令能力表。
//!
//! 插件不再把命令登记到 `/help` 插件里：它们经宿主预置的
//! `fuyubot.host.commands` service 登记，宿主是**唯一**的表主。这样得到三件事：
//!
//! - **归属权威**：`owner` 来自 ABI v6 的调用者栈（`current_plugin`），入参里没有
//!   `plugin` 字段可伪造；同名命令由别的 owner 注册会被拒绝。
//! - **生命周期统一**：插件卸载 / 加载失败按 owner 回滚，脚本插件按子作用域回滚。
//! - **能力前置**：闸门在分发前就能看到「这条命令至少需要什么角色、允许哪些会话」，
//!   而不是等插件回调里再各自检查。
//!
//! 表里存的是**能力**，不是实现：`/help` 只读它渲染；命令真正的处理仍在各插件。
//!
//! 默认值也按能力走：普通命令默认 `private + group`，`admin` / `owner` 默认只有
//! `private`（免得在群里把主人命令名泄给所有成员）；频道要显式声明。

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde_json::{Value, json};

use crate::cache::ChatKind;

/// 命令要求的最低角色。
///
/// `chat_admin` 是**会话角色**：群/频道里的管理员，不等于全局 `admin`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandRole {
    /// 任何能触发命令的人都能用（默认）。
    #[default]
    User,
    /// 需要**本会话**的管理员身份（群主 / 管理员 / 频道发帖者）。
    ChatAdmin,
    /// 需要全局管理员（`[roles].admins`）。
    Admin,
    /// 需要主人（`[roles].owners`；user 账户在 owners 为空时默认自己是主人）。
    Owner,
}

impl CommandRole {
    /// 从命令表入参解析：缺省 = `user`。
    pub fn parse(value: Option<&Value>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::User);
        };
        let Some(raw) = value.as_str() else {
            return Err("role 必须是字符串".to_owned());
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "user" => Ok(Self::User),
            "chat_admin" | "chat-admin" | "chatadmin" => Ok(Self::ChatAdmin),
            "admin" => Ok(Self::Admin),
            "owner" => Ok(Self::Owner),
            other => Err(format!(
                "role = \"{other}\" 认不出来（user / chat_admin / admin / owner）"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ChatAdmin => "chat_admin",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// 闸门能判的那部分。
    ///
    /// `chat_admin` 由缓存给出 `Some(true/false)` 时直接判；`None`（缓存里没有）
    /// 放行，交给命令处理器（或 `/help` 的回复线程）再判——闸门在接收循环上不能等 TDLib。
    pub fn allows_gate(self, global: GlobalRole, chat_admin: Option<bool>) -> bool {
        match self {
            Self::User => true,
            Self::ChatAdmin => chat_admin.unwrap_or(true),
            Self::Admin => global >= GlobalRole::Admin,
            Self::Owner => global == GlobalRole::Owner,
        }
    }
}

/// 配置算出来的全局角色。顺序有意义：`User < Admin < Owner`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GlobalRole {
    User,
    Admin,
    Owner,
}

impl GlobalRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }
}

/// 命令允许出现的会话类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandScope {
    Private,
    Group,
    Channel,
}

impl CommandScope {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "private" | "pm" | "dm" => Some(Self::Private),
            "group" | "supergroup" => Some(Self::Group),
            "channel" => Some(Self::Channel),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Channel => "channel",
        }
    }

    /// 权限表用：这个 scope 是否允许该会话类型。
    pub fn matches(self, kind: ChatKind) -> bool {
        matches!(
            (self, kind),
            (Self::Private, ChatKind::Private)
                | (Self::Group, ChatKind::Group)
                | (Self::Channel, ChatKind::Channel)
        )
    }
}

/// 表里的一条命令。
#[derive(Debug, Clone)]
pub struct CommandEntry {
    /// 已归一化成小写的命令名（不带 `/`）。
    pub name: String,
    pub description: String,
    /// 登记它的插件名；宿主自己登记时是 `None`。
    pub plugin: Option<String>,
    /// 宿主侧 owner id：回滚与防冒名用。
    pub owner: u64,
    /// 子作用域句柄（脚本插件用；普通插件是 0）。
    pub scope: u64,
    /// 至少需要什么角色。
    pub role: CommandRole,
    /// 允许出现在哪些会话。
    pub scopes: Vec<CommandScope>,
    /// 不在 `/help` 里显示，但仍按能力表执行。
    pub hidden: bool,
}

impl CommandEntry {
    /// 闸门用：只有 `chat_id`，分不出群和频道，所以负数一律按「群或频道」放行；
    /// 精确到频道由命令自己（或 `/help` 的回复线程）再判。
    pub fn scope_allows_chat_id(&self, chat_id: i64) -> bool {
        if chat_id > 0 {
            self.scopes.contains(&CommandScope::Private)
        } else {
            self.scopes
                .iter()
                .any(|scope| matches!(scope, CommandScope::Group | CommandScope::Channel))
        }
    }

    /// 给 service 的 JSON 形状。`plugin` 没有就**不写**（缺字段 ≠ 空串）。
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "name": &self.name,
            "description": &self.description,
            "role": self.role.as_str(),
            "scopes": self.scopes.iter().map(|scope| scope.as_str()).collect::<Vec<_>>(),
            "hidden": self.hidden,
        });
        if let Some(plugin) = &self.plugin {
            value["plugin"] = json!(plugin);
        }
        value
    }
}

/// 没写 `scopes` 时的默认值。
///
/// 普通命令：私聊 + 群（频道要显式声明——老插件从没在频道里工作过）。
/// `admin` / `owner` 命令：**只给私聊**，免得在群里把主人命令名泄给所有成员；
/// 真要在群里用，显式写 `scopes`。
fn default_scopes(role: CommandRole) -> Vec<CommandScope> {
    match role {
        CommandRole::Admin | CommandRole::Owner => vec![CommandScope::Private],
        _ => vec![CommandScope::Private, CommandScope::Group],
    }
}

/// 宿主侧的命令能力表。
#[derive(Debug, Default)]
pub struct CommandRegistry {
    entries: Mutex<BTreeMap<String, CommandEntry>>,
}

impl CommandRegistry {
    /// 登记 / 更新一条命令。
    ///
    /// `owner` / `scope` 由宿主从调用者栈与作用域栈取出，**不读入参**。
    /// 同名命令只允许原 owner 更新；别的 owner 来登记直接拒绝，避免插件互相覆盖。
    pub fn register(
        &self,
        owner: u64,
        scope: u64,
        plugin: Option<String>,
        input: &Value,
    ) -> Result<CommandEntry, String> {
        let object = input
            .as_object()
            .ok_or_else(|| "入参必须是 JSON 对象".to_owned())?;
        let name = command_name(object.get("name"))?;
        let description = object
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        let role = CommandRole::parse(object.get("role"))?;
        let scopes = match object.get("scopes") {
            Some(value) => parse_scopes(value)?,
            None => default_scopes(role),
        };
        let hidden = match object.get("hidden") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "hidden 必须是 true / false".to_owned())?,
        };

        let mut entries = self.lock();
        if let Some(existing) = entries.get(&name) {
            if existing.owner != owner {
                return Err(format!(
                    "命令 /{name} 已由 {} 注册",
                    existing.plugin.as_deref().unwrap_or("另一个插件")
                ));
            }
        }
        let entry = CommandEntry {
            name: name.clone(),
            description,
            plugin,
            owner,
            scope,
            role,
            scopes,
            hidden,
        };
        entries.insert(name, entry.clone());
        Ok(entry)
    }

    /// 注销一条命令。`owner == 0` 是宿主自己，允许注销任何一条；插件只能注销自己的。
    pub fn unregister(&self, owner: u64, name: &str) -> bool {
        let name = normalize_name(name);
        let mut entries = self.lock();
        match entries.get(&name) {
            Some(entry) if owner == 0 || entry.owner == owner => {
                entries.remove(&name);
                true
            }
            _ => false,
        }
    }

    /// 全部命令（按名字排序，`BTreeMap` 天然稳定）。
    pub fn list(&self) -> Vec<CommandEntry> {
        self.lock().values().cloned().collect()
    }

    /// 按名字查一条（闸门用）。
    pub fn get(&self, name: &str) -> Option<CommandEntry> {
        self.lock().get(&normalize_name(name)).cloned()
    }

    /// 插件卸载 / 加载失败：整批摘掉。
    pub fn remove_owner(&self, owner: u64) {
        if owner == 0 {
            return;
        }
        self.lock().retain(|_, entry| entry.owner != owner);
    }

    /// 脚本子作用域回滚：只摘这个作用域里登记的（`scope = 0` 是插件根，不动）。
    pub fn remove_scope(&self, scope: u64) {
        if scope == 0 {
            return;
        }
        self.lock().retain(|_, entry| entry.scope != scope);
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, CommandEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// 命令名：非空、只允许 `a-z0-9_`，统一小写。
///
/// 和 Telegram 的命令语法一致；挡住空格 / `@` / 控制字符，表里就不会出现
/// 「名字对不上、但又能被某个前缀解析出来」的条目。
fn command_name(value: Option<&Value>) -> Result<String, String> {
    let Some(raw) = value.and_then(Value::as_str) else {
        return Err("缺 name 或不是字符串".to_owned());
    };
    let name = raw.trim().trim_start_matches('/').to_ascii_lowercase();
    if name.is_empty() {
        return Err("name 不能为空".to_owned());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "命令名 \"{name}\" 只能由字母、数字、下划线组成（Telegram 的命令语法）"
        ));
    }
    Ok(name)
}

fn normalize_name(name: &str) -> String {
    name.trim().trim_start_matches('/').to_ascii_lowercase()
}

/// 显式给了 `scopes` 就必须是非空数组。
fn parse_scopes(value: &Value) -> Result<Vec<CommandScope>, String> {
    let Some(items) = value.as_array() else {
        return Err("scopes 必须是数组，例如 [\"private\", \"group\"]".to_owned());
    };
    let mut scopes = Vec::new();
    for item in items {
        let Some(raw) = item.as_str() else {
            return Err(format!("scopes 里的 {item} 不是字符串"));
        };
        let Some(scope) = CommandScope::parse(raw) else {
            return Err(format!(
                "scopes 里的 \"{raw}\" 认不出来（private / group / channel）"
            ));
        };
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    if scopes.is_empty() {
        return Err("scopes 不能是空数组（命令会永远不可用）".to_owned());
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(registry: &CommandRegistry, owner: u64, input: Value) -> CommandEntry {
        registry
            .register(owner, 0, Some(format!("p{owner}")), &input)
            .unwrap()
    }

    #[test]
    fn defaults_are_user_private_and_group() {
        let registry = CommandRegistry::default();
        let entry = register(&registry, 1, json!({"name": "Ping", "description": "探活"}));
        assert_eq!(entry.name, "ping");
        assert_eq!(entry.role, CommandRole::User);
        assert_eq!(
            entry.scopes,
            vec![CommandScope::Private, CommandScope::Group]
        );
        assert!(!entry.hidden);
    }

    /// 主人 / 管理员命令默认只给私聊，避免在群里把命令名泄给所有成员。
    #[test]
    fn privileged_commands_default_to_private_only() {
        let registry = CommandRegistry::default();
        let entry = register(&registry, 1, json!({"name": "config", "role": "owner"}));
        assert_eq!(entry.scopes, vec![CommandScope::Private]);
        let entry = register(&registry, 1, json!({"name": "reload", "role": "admin"}));
        assert_eq!(entry.scopes, vec![CommandScope::Private]);
    }

    #[test]
    fn another_owner_cannot_overwrite_a_command() {
        let registry = CommandRegistry::default();
        register(&registry, 1, json!({"name": "ping"}));
        let error = registry
            .register(2, 0, Some("p2".to_owned()), &json!({"name": "ping"}))
            .unwrap_err();
        assert!(error.contains("已由 p1 注册"), "{error}");
    }

    #[test]
    fn same_owner_can_update_metadata() {
        let registry = CommandRegistry::default();
        register(&registry, 1, json!({"name": "ping", "description": "旧"}));
        let entry = register(
            &registry,
            1,
            json!({"name": "ping", "description": "新", "role": "admin"}),
        );
        assert_eq!(entry.description, "新");
        assert_eq!(entry.role, CommandRole::Admin);
    }

    #[test]
    fn rollback_by_owner_and_scope() {
        let registry = CommandRegistry::default();
        registry
            .register(1, 0, Some("a".to_owned()), &json!({"name": "one"}))
            .unwrap();
        registry
            .register(2, 7, Some("b".to_owned()), &json!({"name": "two"}))
            .unwrap();
        registry.remove_scope(7);
        assert!(registry.get("two").is_none());
        registry.remove_owner(1);
        assert!(registry.get("one").is_none());
        assert!(registry.list().is_empty());
    }

    #[test]
    fn invalid_metadata_is_rejected_with_a_reason() {
        let registry = CommandRegistry::default();
        for input in [
            json!({"name": ""}),
            json!({"name": "bad name"}),
            json!({"name": "ok", "role": "root"}),
            json!({"name": "ok", "scopes": []}),
            json!({"name": "ok", "scopes": ["private", "space"]}),
            json!({"name": "ok", "hidden": "yes"}),
        ] {
            let error = registry
                .register(1, 0, Some("p".to_owned()), &input)
                .unwrap_err();
            assert!(!error.is_empty(), "{input} 应该被拒绝");
        }
    }

    #[test]
    fn gate_allows_chat_admin_for_the_handler_to_check() {
        assert!(CommandRole::ChatAdmin.allows_gate(GlobalRole::User, None));
        assert!(CommandRole::ChatAdmin.allows_gate(GlobalRole::User, Some(true)));
        assert!(!CommandRole::ChatAdmin.allows_gate(GlobalRole::User, Some(false)));
        assert!(!CommandRole::Admin.allows_gate(GlobalRole::User, None));
        assert!(CommandRole::Admin.allows_gate(GlobalRole::Admin, None));
        assert!(!CommandRole::Owner.allows_gate(GlobalRole::Admin, None));
    }

    #[test]
    fn scope_checks_by_chat_id_sign() {
        let registry = CommandRegistry::default();
        let entry = register(&registry, 1, json!({"name": "pm", "scopes": ["private"]}));
        assert!(entry.scope_allows_chat_id(7));
        assert!(!entry.scope_allows_chat_id(-100));
        let group = register(&registry, 1, json!({"name": "grp", "scopes": ["group"]}));
        assert!(group.scope_allows_chat_id(-100));
        assert!(!group.scope_allows_chat_id(7));
    }
}
