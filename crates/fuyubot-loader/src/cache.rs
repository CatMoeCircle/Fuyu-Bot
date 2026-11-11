//! 宿主侧的 TDLib 全量对象缓存。
//!
//! 缓存的是 TDLib 原始 JSON（`serde_json::Value`），不做精简：`/help`、命令处理器
//! 和 `Simplify` 拿到的形状与直接请求 TDLib 完全一致。内存由**每种类型的条数上限**
//! 兜住（默认 100），不靠 TTL 控制占用。
//!
//! - `chat` / `user` / `basicGroup` / `supergroup` / `chatMember` / `community` /
//!   `communityFullInfo` 各一张表；
//! - `updateNewChat` / `updateChat*` / `updateUser` / `updateBasicGroup` /
//!   `updateSupergroup` / `updateChatMember` / `updateCommunity*` 直接刷新；
//! - `updateChatFolders` 单独存一份 chat list 状态；
//! - `updateConnectionState` 决定「缓存未命中时还要不要去问 TDLib」。
//!
//! 不做逐资源的「还活着吗」探测：`getChat` 就是探测，命中后再发一次等于缓存失效。
//! 需要新鲜度的是成员权限，所以只有 `chatMember` 带 30 秒 TTL。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use serde_json::{Value, json};

/// 宿主缓存 service 的名字。`Simplify` 经它做「内存优先」。
pub const CACHE_SERVICE: &str = "fuyubot.host.cache";

/// 每种类型默认保留多少条。全量 `Value` 在 100 条这个量级是几 MiB，不需要 TTL。
pub const DEFAULT_CAPACITY: usize = 100;

/// 缓存里的一类对象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Chat,
    User,
    BasicGroup,
    Supergroup,
    ChatMember,
    Community,
    CommunityFullInfo,
}

impl CacheKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "user" => Some(Self::User),
            "basic_group" | "basicgroup" => Some(Self::BasicGroup),
            "supergroup" => Some(Self::Supergroup),
            "chat_member" | "chatmember" => Some(Self::ChatMember),
            "community" => Some(Self::Community),
            "community_full_info" | "communityfullinfo" => Some(Self::CommunityFullInfo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::User => "user",
            Self::BasicGroup => "basic_group",
            Self::Supergroup => "supergroup",
            Self::ChatMember => "chat_member",
            Self::Community => "community",
            Self::CommunityFullInfo => "community_full_info",
        }
    }

    fn key(self, id: i64, user_id: Option<i64>) -> String {
        match self {
            Self::ChatMember => format!("chat_member:{id}:{}", user_id.unwrap_or(0)),
            _ => format!("{}:{id}", self.as_str()),
        }
    }
}

/// 一次查询的结果。`online` 即使缓存关闭也会返回，调用方据此决定要不要等 TDLib。
#[derive(Debug, Clone)]
pub struct Lookup {
    pub value: Option<Value>,
    pub online: bool,
}

/// 会话类型（权限表判 scope 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Private,
    Group,
    Channel,
}

/// `chat_member` 条目的 TTL：权限判断不能拿太旧的缓存。
pub const CHAT_MEMBER_TTL_MS: u64 = 30_000;

struct Entry {
    kind: CacheKind,
    value: Value,
    inserted_at: Instant,
    last_used: u64,
}

struct Inner {
    entries: HashMap<String, Entry>,
    /// chat list 索引：`list key -> chat_id -> 轻量条目`。
    ///
    /// **不参与 `[cache].capacity`**：列表要完整，不能因为 chat 对象被 LRU 淘汰
    /// 就从列表里消失。这里只存 id / order / title / type，不存完整 Chat；
    /// 需要完整对象时再 `getChat`（命中 chat 缓存就直接用）。
    lists: HashMap<String, HashMap<i64, ChatListEntry>>,
    /// `updateChatFolders` 的最新快照（chat list 定义与顺序参数）。
    chat_folders: Option<Value>,
    tick: u64,
}

/// chat list 里的一条轻量记录。完整 `Chat` 是否还在 100 条缓存里由查询时决定。
#[derive(Debug, Clone)]
struct ChatListEntry {
    chat_id: i64,
    order: i64,
    is_pinned: bool,
    title: String,
    kind: Option<Value>,
}

impl ChatListEntry {
    /// 列表方法只返回精简条目：**不夹带完整 Chat**。
    ///
    /// 要完整对象走 `getChat` / `Simplify::chat`，那条路不受列表索引影响。
    fn to_json(&self) -> Value {
        let mut value = json!({
            "@type": "chatListEntry",
            "chat_id": self.chat_id,
            "order": self.order,
            "is_pinned": self.is_pinned,
            "title": self.title,
        });
        if let Some(kind) = &self.kind {
            value["type"] = kind.clone();
        }
        value
    }
}

/// 宿主缓存。内部一把锁：所有操作只做内存读写，绝不在锁里碰 TDLib。
pub struct Cache {
    inner: Mutex<Inner>,
    enabled: AtomicBool,
    online: AtomicBool,
    capacity: AtomicUsize,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Cache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                lists: HashMap::new(),
                chat_folders: None,
                tick: 0,
            }),
            enabled: AtomicBool::new(true),
            online: AtomicBool::new(true),
            capacity: AtomicUsize::new(DEFAULT_CAPACITY),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 每种类型的上限。太小会让热门对象反复抖动，上限 4096。
    pub fn set_capacity(&self, capacity: usize) {
        self.capacity
            .store(capacity.clamp(1, 4096), Ordering::Relaxed);
    }

    pub fn capacity(&self) -> usize {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn set_online(&self, online: bool) {
        self.online.store(online, Ordering::Relaxed);
    }

    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.entries.clear();
        inner.lists.clear();
        inner.chat_folders = None;
    }

    /// 读一条。`ttl_ms == 0` 表示不按时间过期（chat / user / group 走这条）。
    pub fn get(&self, kind: CacheKind, id: i64, user_id: Option<i64>, ttl_ms: u64) -> Lookup {
        let online = self.online();
        if !self.enabled() {
            return Lookup {
                value: None,
                online,
            };
        }
        let key = kind.key(id, user_id);
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let expired = inner.entries.get(&key).is_some_and(|entry| {
            ttl_ms > 0 && entry.inserted_at.elapsed().as_millis() as u64 >= ttl_ms
        });
        if expired {
            inner.entries.remove(&key);
            return Lookup {
                value: None,
                online,
            };
        }
        let Some(entry) = inner.entries.get_mut(&key) else {
            return Lookup {
                value: None,
                online,
            };
        };
        entry.last_used = tick;
        Lookup {
            value: Some(entry.value.clone()),
            online,
        }
    }

    /// 写一条。`value == null` 等于删除。
    pub fn put(&self, kind: CacheKind, id: i64, user_id: Option<i64>, value: Value) {
        if !self.enabled() {
            return;
        }
        if value.is_null() {
            self.remove(kind, id, user_id);
            return;
        }
        // Chat 的列表归属是 `positions`；写进对象缓存时顺手同步独立索引。
        let list_sync = (kind == CacheKind::Chat).then(|| {
            (
                value
                    .get("positions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                value.get("title").cloned(),
                value.get("type").cloned(),
            )
        });
        let key = kind.key(id, user_id);
        let capacity = self.capacity();
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if !inner.entries.contains_key(&key) && inner.count_kind(kind) >= capacity {
            inner.evict_one(kind);
        }
        inner.entries.insert(
            key,
            Entry {
                kind,
                value,
                inserted_at: Instant::now(),
                last_used: tick,
            },
        );
        drop(inner);
        if let Some((positions, title, kind)) = list_sync {
            self.replace_positions(id, &positions, title.as_ref(), kind.as_ref());
        }
    }

    /// 就地改一条。缓存里没有就什么都不做（下次 `getChat` 会补全量对象）。
    pub fn update(
        &self,
        kind: CacheKind,
        id: i64,
        user_id: Option<i64>,
        change: impl FnOnce(&mut Value),
    ) {
        if !self.enabled() {
            return;
        }
        let key = kind.key(id, user_id);
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let Some(entry) = inner.entries.get_mut(&key) else {
            return;
        };
        change(&mut entry.value);
        entry.last_used = tick;
    }

    pub fn remove(&self, kind: CacheKind, id: i64, user_id: Option<i64>) {
        let mut inner = self.lock();
        inner.entries.remove(&kind.key(id, user_id));
        if kind == CacheKind::Chat {
            for entries in inner.lists.values_mut() {
                entries.remove(&id);
            }
        }
    }

    /// 权限表用：会话类型。私聊只看 id 符号；群 / 频道要缓存里有 Chat。
    ///
    /// 返回 `None` = 缓存里没有，调用方要么自己 `getChat`，要么按「未知」放行/交给处理器。
    pub fn chat_kind(&self, chat_id: i64) -> Option<ChatKind> {
        if chat_id > 0 {
            return Some(ChatKind::Private);
        }
        if !self.enabled() {
            return None;
        }
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let Some(entry) = inner.entries.get_mut(&CacheKind::Chat.key(chat_id, None)) else {
            return None;
        };
        entry.last_used = tick;
        chat_kind_of(&entry.value)
    }

    /// 权限表用：某个用户是不是该会话的管理员。
    ///
    /// 只读缓存且带 30 秒 TTL：`Some(true/false)` 是缓存结论，`None` = 缓存里没有，
    /// 调用方应交给命令处理器（或先 `getChatMember` 回填）再判。
    pub fn chat_admin(&self, chat_id: i64, user_id: i64) -> Option<bool> {
        self.get(
            CacheKind::ChatMember,
            chat_id,
            Some(user_id),
            CHAT_MEMBER_TTL_MS,
        )
        .value
        .as_ref()
        .map(is_admin_member)
    }

    /// `updateChatFolders` 的快照；`None` = 还没收到过。
    pub fn chat_folders(&self) -> Option<Value> {
        if !self.enabled() {
            return None;
        }
        self.lock().chat_folders.clone()
    }

    fn set_chat_folders(&self, update: &Value) {
        if !self.enabled() {
            return;
        }
        let state = json!({
            "chat_folders": update.get("chat_folders").cloned().unwrap_or_else(|| json!([])),
            "main_chat_list_position": update.get("main_chat_list_position").cloned().unwrap_or_else(|| json!(0)),
            "are_tags_enabled": update.get("are_tags_enabled").cloned().unwrap_or_else(|| json!(false)),
        });
        let valid: Vec<i64> = state
            .get("chat_folders")
            .and_then(Value::as_array)
            .map(|folders| {
                folders
                    .iter()
                    .filter_map(|folder| folder.get("id").and_then(Value::as_i64))
                    .collect()
            })
            .unwrap_or_default();
        let mut inner = self.lock();
        inner.chat_folders = Some(state);
        // folder 被删掉后，旧索引不该继续留在列表里。
        inner
            .lists
            .retain(|key, _| match key.strip_prefix("folder:") {
                Some(raw) => raw
                    .parse::<i64>()
                    .ok()
                    .is_some_and(|id| valid.contains(&id)),
                None => true,
            });
    }

    /// 列表分组：main / archive / 每个 chat folder。
    ///
    /// folder 的标题 / 图标来自 `updateChatFolders` 的快照；每项带 `list`，
    /// 可以直接传给 [`Self::chats_in_list`]。
    pub fn chat_list_groups(&self) -> Vec<Value> {
        if !self.enabled() {
            return Vec::new();
        }
        let inner = self.lock();
        let mut groups = vec![
            group_json(
                "main",
                "全部",
                json!({"@type": "chatListMain"}),
                inner.lists.get("main"),
                None,
            ),
            group_json(
                "archive",
                "归档",
                json!({"@type": "chatListArchive"}),
                inner.lists.get("archive"),
                None,
            ),
        ];
        if let Some(folders) = inner
            .chat_folders
            .as_ref()
            .and_then(|state| state.get("chat_folders"))
            .and_then(Value::as_array)
        {
            for folder in folders {
                let Some(id) = folder.get("id").and_then(Value::as_i64) else {
                    continue;
                };
                let title = folder_title(folder)
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| format!("分组 {id}"));
                groups.push(group_json(
                    "folder",
                    &title,
                    json!({"@type": "chatListFolder", "chat_folder_id": id}),
                    inner.lists.get(&format!("folder:{id}")),
                    Some(folder.clone()),
                ));
            }
        }
        groups
    }

    /// 某个 chat list 的有序成员。
    ///
    /// 读的是**独立索引**，不扫描 100 条 chat 缓存：列表里的 chat 即使被 LRU 淘汰，
    /// 条目仍然在（`不参加 [cache].capacity`）。每项是精简的
    /// `{"@type":"chatListEntry","chat_id":…,"order":…,"is_pinned":…,"title":…,"type":…}`，
    /// **不夹带完整 Chat**；完整对象走 `getChat` / `Simplify::chat`。
    pub fn chats_in_list(&self, list: &Value, limit: usize) -> Vec<Value> {
        if !self.enabled() {
            return Vec::new();
        }
        let limit = limit.clamp(1, 100_000);
        let inner = self.lock();
        let Some(entries) = inner.lists.get(&list_key(list)) else {
            return Vec::new();
        };
        let mut rows: Vec<&ChatListEntry> =
            entries.values().filter(|entry| entry.order > 0).collect();
        rows.sort_by(|a, b| {
            b.order
                .cmp(&a.order)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        rows.into_iter()
            .take(limit)
            .map(ChatListEntry::to_json)
            .collect()
    }

    /// `updateNewChat` / `updateChatLastMessage` / `updateChatDraftMessage`：
    /// `positions` 是**完整**的列表归属，先清掉这个 chat 在所有 list 里的旧条目，再重建。
    fn replace_positions(
        &self,
        chat_id: i64,
        positions: &[Value],
        title: Option<&Value>,
        kind: Option<&Value>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut inner = self.lock();
        let cached = inner.entries.get(&CacheKind::Chat.key(chat_id, None));
        // chat 对象可能已被 LRU 淘汰，但列表条目还在：保留它已有的 title/type，
        // 不要因为一次 position 更新把列表里的标题清空。
        let existing = inner
            .lists
            .values()
            .find_map(|entries| entries.get(&chat_id))
            .cloned();
        let title = title
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                cached.and_then(|entry| entry.value.get("title")?.as_str().map(str::to_owned))
            })
            .or_else(|| existing.as_ref().map(|entry| entry.title.clone()))
            .unwrap_or_default();
        let kind = kind
            .cloned()
            .or_else(|| cached.and_then(|entry| entry.value.get("type").cloned()))
            .or_else(|| existing.as_ref().and_then(|entry| entry.kind.clone()));
        for entries in inner.lists.values_mut() {
            entries.remove(&chat_id);
        }
        for position in positions {
            let order = position.get("order").and_then(Value::as_i64).unwrap_or(0);
            if order <= 0 {
                continue;
            }
            let Some(list) = position.get("list") else {
                continue;
            };
            inner.lists.entry(list_key(list)).or_default().insert(
                chat_id,
                ChatListEntry {
                    chat_id,
                    order,
                    is_pinned: position
                        .get("is_pinned")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    title: title.clone(),
                    kind: kind.clone(),
                },
            );
        }
    }

    /// `updateChatPosition`：只动**这一个** list；`order == 0` 表示移除。
    fn sync_position(&self, chat_id: i64, position: &Value) {
        if !self.enabled() {
            return;
        }
        let Some(list) = position.get("list") else {
            return;
        };
        let key = list_key(list);
        let order = position.get("order").and_then(Value::as_i64).unwrap_or(0);
        let mut inner = self.lock();
        let cached = inner.entries.get(&CacheKind::Chat.key(chat_id, None));
        let existing = inner
            .lists
            .get(&key)
            .and_then(|entries| entries.get(&chat_id))
            .cloned();
        let title = cached
            .and_then(|entry| entry.value.get("title")?.as_str().map(str::to_owned))
            .or_else(|| existing.as_ref().map(|entry| entry.title.clone()))
            .unwrap_or_default();
        let kind = cached
            .and_then(|entry| entry.value.get("type").cloned())
            .or_else(|| existing.and_then(|entry| entry.kind));
        if order <= 0 {
            if let Some(entries) = inner.lists.get_mut(&key) {
                entries.remove(&chat_id);
            }
            return;
        }
        inner.lists.entry(key).or_default().insert(
            chat_id,
            ChatListEntry {
                chat_id,
                order,
                is_pinned: position
                    .get("is_pinned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                title,
                kind,
            },
        );
    }

    fn rename_in_lists(&self, chat_id: i64, title: &str) {
        if !self.enabled() {
            return;
        }
        let mut inner = self.lock();
        for entries in inner.lists.values_mut() {
            if let Some(entry) = entries.get_mut(&chat_id) {
                entry.title = title.to_owned();
            }
        }
    }

    fn add_to_list(&self, chat_id: i64, list: &Value) {
        if !self.enabled() {
            return;
        }
        let key = list_key(list);
        let mut inner = self.lock();
        let cached = inner.entries.get(&CacheKind::Chat.key(chat_id, None));
        let existing = inner
            .lists
            .get(&key)
            .and_then(|entries| entries.get(&chat_id))
            .cloned();
        let title = cached
            .and_then(|entry| entry.value.get("title")?.as_str().map(str::to_owned))
            .or_else(|| existing.as_ref().map(|entry| entry.title.clone()))
            .unwrap_or_default();
        let kind = cached
            .and_then(|entry| entry.value.get("type").cloned())
            .or_else(|| existing.as_ref().and_then(|entry| entry.kind.clone()));
        inner.lists.entry(key).or_default().insert(
            chat_id,
            ChatListEntry {
                chat_id,
                // `updateChatAddedToList` 不带 order：已有 order 就保留，
                // 没有就先记 0，等 `updateChatPosition` 到了再变成可见。
                order: existing.as_ref().map(|entry| entry.order).unwrap_or(0),
                is_pinned: existing.as_ref().is_some_and(|entry| entry.is_pinned),
                title,
                kind,
            },
        );
    }

    fn remove_from_list(&self, chat_id: i64, list: &Value) {
        if !self.enabled() {
            return;
        }
        let key = list_key(list);
        if let Some(entries) = self.lock().lists.get_mut(&key) {
            entries.remove(&chat_id);
        }
    }

    /// 诊断用：条数 / 开关 / 在线状态 / chat list 快照。
    pub fn stats(&self) -> Value {
        let inner = self.lock();
        let mut counts = serde_json::Map::new();
        for kind in [
            CacheKind::Chat,
            CacheKind::User,
            CacheKind::BasicGroup,
            CacheKind::Supergroup,
            CacheKind::ChatMember,
            CacheKind::Community,
            CacheKind::CommunityFullInfo,
        ] {
            counts.insert(
                kind.as_str().to_owned(),
                json!(
                    inner
                        .entries
                        .values()
                        .filter(|entry| entry.kind == kind)
                        .count()
                ),
            );
        }
        let list_entries: usize = inner.lists.values().map(HashMap::len).sum();
        json!({
            "enabled": self.enabled(),
            "online": self.online(),
            "capacity": self.capacity(),
            "entries": counts,
            "chat_list_entries": list_entries,
            "chat_folders": inner.chat_folders,
        })
    }

    /// 把一条 TDLib 更新应用进缓存。管线在**中间件之前**调它，保证下游看到的是新值。
    pub fn apply_update(&self, update: &Value) {
        let Some(update_type) = update.get("@type").and_then(Value::as_str) else {
            return;
        };

        // 这两个即使缓存关闭也要处理：它们决定「未命中时要不要等 TDLib」。
        match update_type {
            "updateConnectionState" => {
                let state = update
                    .pointer("/state/@type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.set_online(matches!(
                    state,
                    "connectionStateReady" | "connectionStateUpdating"
                ));
                return;
            }
            "updateAuthorizationState" => {
                let state = update
                    .pointer("/authorization_state/@type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if matches!(
                    state,
                    "authorizationStateLoggingOut" | "authorizationStateClosed"
                ) {
                    self.clear();
                    self.set_online(false);
                }
                return;
            }
            _ => {}
        }

        if !self.enabled() {
            return;
        }

        match update_type {
            "updateNewChat" => {
                if let Some(chat) = update.get("chat")
                    && let Some(id) = chat_id(chat)
                {
                    self.put(CacheKind::Chat, id, None, chat.clone());
                }
            }
            "updateUser" => {
                if let Some(user) = update.get("user")
                    && let Some(id) = user.get("id").and_then(Value::as_i64)
                {
                    self.put(CacheKind::User, id, None, user.clone());
                }
            }
            "updateUserStatus" => {
                if let (Some(id), Some(status)) = (
                    update.get("user_id").and_then(Value::as_i64),
                    update.get("status"),
                ) {
                    self.update(CacheKind::User, id, None, |user| {
                        user["status"] = status.clone();
                    });
                }
            }
            "updateBasicGroup" => {
                if let Some(group) = update.get("basic_group")
                    && let Some(id) = group.get("id").and_then(Value::as_i64)
                {
                    self.put(CacheKind::BasicGroup, id, None, group.clone());
                }
            }
            "updateSupergroup" => {
                if let Some(group) = update.get("supergroup")
                    && let Some(id) = group.get("id").and_then(Value::as_i64)
                {
                    self.put(CacheKind::Supergroup, id, None, group.clone());
                }
            }
            "updateChatMember" => {
                // 1.8.69 是 `new_chat_member`；老版本叫 `member`，两个都认。
                let member = update
                    .get("new_chat_member")
                    .or_else(|| update.get("member"));
                if let (Some(chat_id), Some(member)) =
                    (update.get("chat_id").and_then(Value::as_i64), member)
                    && let Some(user_id) =
                        member.pointer("/member_id/user_id").and_then(Value::as_i64)
                {
                    self.put(
                        CacheKind::ChatMember,
                        chat_id,
                        Some(user_id),
                        member.clone(),
                    );
                }
            }
            "updateCommunity" => {
                if let Some(community) = update.get("community")
                    && let Some(id) = community.get("id").and_then(Value::as_i64)
                {
                    self.put(CacheKind::Community, id, None, community.clone());
                }
            }
            "updateCommunityFullInfo" => {
                if let (Some(id), Some(info)) = (
                    update.get("community_id").and_then(Value::as_i64),
                    update.get("community_full_info"),
                ) {
                    self.put(CacheKind::CommunityFullInfo, id, None, info.clone());
                }
            }
            "updateChatFolders" => self.set_chat_folders(update),
            // `updateChatMember` / `updateChatFolders` 已在上面处理；其余 `updateChat*`
            // 都是 Chat 的字段更新。
            update_type if update_type.starts_with("updateChat") => {
                self.apply_chat_update(update, update_type);
            }
            _ => {}
        }
    }

    fn apply_chat_update(&self, update: &Value, update_type: &str) {
        let Some(chat_id) = update.get("chat_id").and_then(Value::as_i64) else {
            return;
        };
        // 这些不是 `Chat` 字段：动作、欢迎语、加入结果、故事、收入、boost、在线人数
        // 都是独立对象或一次性事件。写进 Chat 只会污染缓存。
        if matches!(
            update_type,
            "updateChatAction"
                | "updateChatWelcomeMessages"
                | "updateChatJoinResult"
                | "updateChatActiveStories"
                | "updateChatRevenueAmount"
                | "updateChatBoost"
                | "updateChatOnlineMemberCount"
        ) {
            return;
        }
        match update_type {
            "updateChatPosition" => {
                let Some(position) = update.get("position") else {
                    return;
                };
                self.update(CacheKind::Chat, chat_id, None, |chat| {
                    merge_position(chat, position);
                });
                self.sync_position(chat_id, position);
            }
            "updateChatTitle" => {
                let title = update.get("title").and_then(Value::as_str).unwrap_or("");
                self.update(CacheKind::Chat, chat_id, None, |chat| {
                    chat["title"] = json!(title);
                });
                self.rename_in_lists(chat_id, title);
            }
            // `Chat.reply_markup_message_id` 是 id；更新里给的是完整 `message`（可能 null）。
            "updateChatReplyMarkup" => {
                let id = update
                    .get("reply_markup_message")
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                self.update(CacheKind::Chat, chat_id, None, |chat| {
                    chat["reply_markup_message_id"] = json!(id);
                });
            }
            "updateChatAddedToList" => {
                if let Some(list) = update.get("chat_list") {
                    self.update(CacheKind::Chat, chat_id, None, |chat| {
                        merge_chat_list(chat, list, true);
                    });
                    self.add_to_list(chat_id, list);
                }
            }
            "updateChatRemovedFromList" => {
                if let Some(list) = update.get("chat_list") {
                    self.update(CacheKind::Chat, chat_id, None, |chat| {
                        merge_chat_list(chat, list, false);
                    });
                    self.remove_from_list(chat_id, list);
                }
            }
            _ => {
                self.update(CacheKind::Chat, chat_id, None, |chat| {
                    let (Some(source), Some(target)) = (update.as_object(), chat.as_object_mut())
                    else {
                        return;
                    };
                    for (key, value) in source {
                        if key == "@type" || key == "chat_id" {
                            continue;
                        }
                        target.insert(key.clone(), value.clone());
                    }
                });
                // `updateChatLastMessage` / `updateChatDraftMessage` 带完整 positions。
                if let Some(positions) = update.get("positions").and_then(Value::as_array) {
                    self.replace_positions(chat_id, positions, None, None);
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Inner {
    fn count_kind(&self, kind: CacheKind) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.kind == kind)
            .count()
    }

    fn evict_one(&mut self, kind: CacheKind) {
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.kind == kind)
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }
}

fn chat_id(chat: &Value) -> Option<i64> {
    chat.get("id").and_then(Value::as_i64)
}

fn chat_kind_of(chat: &Value) -> Option<ChatKind> {
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

/// `chatMemberStatusCreator` / `chatMemberStatusAdministrator` 算管理员。
fn is_admin_member(member: &Value) -> bool {
    matches!(
        member
            .pointer("/status/@type")
            .and_then(Value::as_str)
            .unwrap_or(""),
        "chatMemberStatusCreator" | "chatMemberStatusAdministrator"
    )
}

fn group_json(
    kind: &str,
    title: &str,
    list: Value,
    entries: Option<&HashMap<i64, ChatListEntry>>,
    folder: Option<Value>,
) -> Value {
    let count = entries.map_or(0, |entries| {
        entries.values().filter(|entry| entry.order > 0).count()
    });
    json!({
        "@type": "chatListGroup",
        "kind": kind,
        "title": title,
        "list": list,
        "count": count,
        "folder": folder,
    })
}

/// `chatFolderInfo.name.text` 是 `formattedText`；取不到就退回 `None`。
fn folder_title(folder: &Value) -> Option<String> {
    folder
        .pointer("/name/text/text")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `ChatList` 的稳定键：main / archive / folder:<id>。
fn list_key(list: &Value) -> String {
    match list.get("@type").and_then(Value::as_str) {
        Some("chatListMain") => "main".to_owned(),
        Some("chatListArchive") => "archive".to_owned(),
        Some("chatListFolder") => format!(
            "folder:{}",
            list.get("chat_folder_id")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        ),
        _ => list.to_string(),
    }
}

/// `updateChatAddedToList` / `updateChatRemovedFromList` 只给 `ChatList`，没有 order；
/// 维护 `Chat.chat_lists`，移除时顺手把对应 `positions` 也摘掉。
fn merge_chat_list(chat: &mut Value, list: &Value, add: bool) {
    let Some(lists) = chat.get_mut("chat_lists").and_then(Value::as_array_mut) else {
        return;
    };
    if add {
        if !lists.iter().any(|candidate| candidate == list) {
            lists.push(list.clone());
        }
        return;
    }
    lists.retain(|candidate| candidate != list);
    if let Some(positions) = chat.get_mut("positions").and_then(Value::as_array_mut) {
        positions.retain(|position| position.get("list") != Some(list));
    }
}

/// `updateChatPosition` 是**单个** position：`order == 0` 表示从该 list 移除，
/// 否则按 `list` 替换或追加。
fn merge_position(chat: &mut Value, position: &Value) {
    let Some(positions) = chat.get_mut("positions").and_then(Value::as_array_mut) else {
        return;
    };
    let list = position.get("list");
    let existing = positions
        .iter()
        .position(|candidate| candidate.get("list") == list);
    let order = position.get("order").and_then(Value::as_i64).unwrap_or(0);
    match (order, existing) {
        (0, Some(index)) => {
            positions.remove(index);
        }
        (0, None) => {}
        (_, Some(index)) => positions[index] = position.clone(),
        (_, None) => positions.push(position.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: i64, title: &str) -> Value {
        json!({
            "@type": "chat",
            "id": id,
            "title": title,
            "positions": [
                {"@type": "chatPosition", "list": {"@type": "chatListMain"}, "order": 100, "is_pinned": false, "source": null}
            ],
            "chat_lists": [{"@type": "chatListMain"}],
            "last_message": null
        })
    }

    #[test]
    fn capacity_is_per_kind_and_evicts_the_least_recently_used() {
        let cache = Cache::new();
        cache.set_capacity(2);
        cache.put(CacheKind::Chat, 1, None, chat(1, "a"));
        cache.put(CacheKind::Chat, 2, None, chat(2, "b"));
        // 摸一下 1：淘汰的应该是 2。
        assert!(cache.get(CacheKind::Chat, 1, None, 0).value.is_some());
        cache.put(CacheKind::Chat, 3, None, chat(3, "c"));
        assert!(cache.get(CacheKind::Chat, 1, None, 0).value.is_some());
        assert!(cache.get(CacheKind::Chat, 2, None, 0).value.is_none());
        assert!(cache.get(CacheKind::Chat, 3, None, 0).value.is_some());

        // 另一种类型有独立配额，不受 chat 影响。
        cache.put(CacheKind::User, 1, None, json!({"@type": "user", "id": 1}));
        assert!(cache.get(CacheKind::User, 1, None, 0).value.is_some());
    }

    #[test]
    fn chat_updates_merge_fields_and_positions() {
        let cache = Cache::new();
        cache.put(CacheKind::Chat, 7, None, chat(7, "旧标题"));
        cache.apply_update(&json!({
            "@type": "updateChatTitle", "chat_id": 7, "title": "新标题"
        }));
        let value = cache.get(CacheKind::Chat, 7, None, 0).value.unwrap();
        assert_eq!(value["title"], "新标题");

        cache.apply_update(&json!({
            "@type": "updateChatPosition",
            "chat_id": 7,
            "position": {"@type": "chatPosition", "list": {"@type": "chatListArchive"}, "order": 50, "is_pinned": false, "source": null}
        }));
        let value = cache.get(CacheKind::Chat, 7, None, 0).value.unwrap();
        assert_eq!(value["positions"].as_array().unwrap().len(), 2);

        cache.apply_update(&json!({
            "@type": "updateChatPosition",
            "chat_id": 7,
            "position": {"@type": "chatPosition", "list": {"@type": "chatListArchive"}, "order": 0, "is_pinned": false, "source": null}
        }));
        let value = cache.get(CacheKind::Chat, 7, None, 0).value.unwrap();
        assert_eq!(value["positions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn chat_member_reply_markup_and_list_updates_are_merged() {
        let cache = Cache::new();
        cache.put(CacheKind::Chat, 7, None, chat(7, "a"));
        cache.apply_update(&json!({
            "@type": "updateChatMember",
            "chat_id": 7,
            "new_chat_member": {"@type": "chatMember", "member_id": {"@type": "messageSenderUser", "user_id": 42}, "status": {"@type": "chatMemberStatusMember"}}
        }));
        assert!(
            cache
                .get(CacheKind::ChatMember, 7, Some(42), 0)
                .value
                .is_some()
        );

        cache.apply_update(&json!({
            "@type": "updateChatReplyMarkup",
            "chat_id": 7,
            "reply_markup_message": {"@type": "message", "id": 99}
        }));
        let value = cache.get(CacheKind::Chat, 7, None, 0).value.unwrap();
        assert_eq!(value["reply_markup_message_id"], 99);

        cache.apply_update(&json!({
            "@type": "updateChatAddedToList",
            "chat_id": 7,
            "chat_list": {"@type": "chatListArchive"}
        }));
        let value = cache.get(CacheKind::Chat, 7, None, 0).value.unwrap();
        assert!(
            value["chat_lists"]
                .as_array()
                .unwrap()
                .iter()
                .any(|list| list["@type"] == "chatListArchive")
        );

        cache.apply_update(&json!({
            "@type": "updateChatRemovedFromList",
            "chat_id": 7,
            "chat_list": {"@type": "chatListArchive"}
        }));
        let value = cache.get(CacheKind::Chat, 7, None, 0).value.unwrap();
        assert!(
            !value["chat_lists"]
                .as_array()
                .unwrap()
                .iter()
                .any(|list| list["@type"] == "chatListArchive")
        );
    }

    #[test]
    fn chat_list_groups_include_folders_and_follow_update_chat_folders() {
        let cache = Cache::new();
        cache.apply_update(&json!({
            "@type": "updateChatFolders",
            "chat_folders": [{
                "@type": "chatFolderInfo",
                "id": 1,
                "name": {"@type": "chatFolderName", "text": {"@type": "formattedText", "text": "工作", "entities": []}, "animate_custom_emoji": false},
                "icon": {"@type": "chatFolderIcon", "name": "工作"},
                "color_id": 0,
                "is_shareable": false,
                "has_my_invite_links": false
            }],
            "main_chat_list_position": 0,
            "are_tags_enabled": false
        }));
        let mut item = chat(1, "a");
        item["positions"] = json!([
            {"@type": "chatPosition", "list": {"@type": "chatListMain"}, "order": 10, "is_pinned": false, "source": null},
            {"@type": "chatPosition", "list": {"@type": "chatListFolder", "chat_folder_id": 1}, "order": 5, "is_pinned": false, "source": null}
        ]);
        cache.put(CacheKind::Chat, 1, None, item);

        let groups = cache.chat_list_groups();
        assert_eq!(groups.len(), 3, "{groups:?}");
        assert_eq!(groups[0]["kind"], "main");
        assert_eq!(groups[0]["count"], 1);
        assert_eq!(groups[1]["kind"], "archive");
        assert_eq!(groups[1]["count"], 0);
        assert_eq!(groups[2]["kind"], "folder");
        assert_eq!(groups[2]["title"], "工作");
        assert_eq!(groups[2]["count"], 1);
        assert_eq!(
            groups[2]["list"],
            json!({"@type": "chatListFolder", "chat_folder_id": 1})
        );

        let entries =
            cache.chats_in_list(&json!({"@type": "chatListFolder", "chat_folder_id": 1}), 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["chat_id"], 1);

        // folder 被删掉：分组和条目都要消失。
        cache.apply_update(&json!({
            "@type": "updateChatFolders",
            "chat_folders": [],
            "main_chat_list_position": 0,
            "are_tags_enabled": false
        }));
        assert_eq!(cache.chat_list_groups().len(), 2);
        assert!(
            cache
                .chats_in_list(&json!({"@type": "chatListFolder", "chat_folder_id": 1}), 10)
                .is_empty()
        );
    }

    #[test]
    fn community_and_folders_are_cached() {
        let cache = Cache::new();
        cache.apply_update(&json!({
            "@type": "updateCommunity",
            "community": {"@type": "community", "id": 5, "name": "社区"}
        }));
        assert_eq!(
            cache.get(CacheKind::Community, 5, None, 0).value.unwrap()["name"],
            "社区"
        );

        cache.apply_update(&json!({
            "@type": "updateChatFolders",
            "chat_folders": [{"@type": "chatFolderInfo", "id": 1, "title": {"@type": "formattedText", "text": "工作", "entities": []}}],
            "main_chat_list_position": 2,
            "are_tags_enabled": true
        }));
        let folders = cache.chat_folders().unwrap();
        assert_eq!(folders["main_chat_list_position"], 2);
        assert_eq!(folders["are_tags_enabled"], true);
        assert_eq!(folders["chat_folders"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn chat_list_is_derived_from_positions() {
        let cache = Cache::new();
        let mut a = chat(1, "a");
        a["positions"][0]["order"] = json!(10);
        let mut b = chat(2, "b");
        b["positions"][0]["order"] = json!(20);
        cache.put(CacheKind::Chat, 1, None, a);
        cache.put(CacheKind::Chat, 2, None, b);

        let list = cache.chats_in_list(&json!({"@type": "chatListMain"}), 10);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["chat_id"], 2, "order 大的在前");
        assert!(
            list[0].get("chat").is_none(),
            "列表条目必须保持精简，不夹带完整 Chat"
        );
    }

    /// 列表条目独立于 100 条 chat 缓存：chat 被 LRU 淘汰后，列表条目仍在。
    #[test]
    fn chat_list_entries_survive_chat_eviction_and_do_not_count_toward_capacity() {
        let cache = Cache::new();
        cache.set_capacity(1);
        cache.put(CacheKind::Chat, 1, None, chat(1, "a"));
        cache.put(CacheKind::Chat, 2, None, chat(2, "b"));

        // chat 1 被挤掉，但列表里两条都还在。
        assert!(cache.get(CacheKind::Chat, 1, None, 0).value.is_none());
        let list = cache.chats_in_list(&json!({"@type": "chatListMain"}), 10);
        assert_eq!(list.len(), 2);
        let evicted = list
            .iter()
            .find(|entry| entry["chat_id"] == 1)
            .expect("被淘汰的 chat 仍应在列表里");
        assert_eq!(evicted["title"], "a", "标题快照要保留");
        assert!(
            evicted.get("chat").is_none(),
            "列表方法不返回完整 Chat，哪怕对象还在缓存"
        );
        assert_eq!(cache.stats()["chat_list_entries"], 2);
    }

    #[test]
    fn connection_state_and_logout_are_handled_even_when_cache_is_off() {
        let cache = Cache::new();
        cache.set_enabled(false);
        cache.apply_update(&json!({
            "@type": "updateConnectionState",
            "state": {"@type": "connectionStateWaitingForNetwork"}
        }));
        assert!(!cache.online());
        let lookup = cache.get(CacheKind::Chat, 1, None, 0);
        assert!(lookup.value.is_none());
        assert!(!lookup.online);

        cache.set_enabled(true);
        cache.put(CacheKind::Chat, 1, None, chat(1, "a"));
        cache.apply_update(&json!({
            "@type": "updateAuthorizationState",
            "authorization_state": {"@type": "authorizationStateLoggingOut"}
        }));
        assert!(cache.get(CacheKind::Chat, 1, None, 0).value.is_none());
    }

    #[test]
    fn ttl_expires_a_single_entry() {
        let cache = Cache::new();
        cache.put(
            CacheKind::ChatMember,
            1,
            Some(2),
            json!({"@type": "chatMember"}),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            cache
                .get(CacheKind::ChatMember, 1, Some(2), 1)
                .value
                .is_none()
        );
    }
}
