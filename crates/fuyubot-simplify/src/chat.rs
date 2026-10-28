//! 群聊管理权、转发、表情反应——三件「照 Bot API 的直觉写一定会错」的事。
//!
//! - **管理权**不在 `user` 上（TDLib 的 `user` 连 `is_bot` 都没有，见 `user.type`）。
//!   真正的答案是 `getChatMember` 给的 `ChatMember.status`：
//!   `chatMemberStatusCreator`（群主）或 `chatMemberStatusAdministrator`（管理员，
//!   具体权限在 `status.rights.*`，即 `chatAdministratorRights` 的那 18 个 `can_*`）。
//!   其它四种状态（`member` / `restricted` / `left` / `banned`）一律不是管理员。
//! - **转发**是 `forwardMessages`（一次一组，不是 Bot API 的 `forwardMessage`），
//!   `message_ids` 必须**严格递增**、最多 100 条；返回的是**还没发完**的临时消息。
//! - **表情反应**有两套方法：用户用 `addMessageReaction`（往已有的反应上加一个），
//!   **bot 只能用** `setMessageReactions`（整组覆盖，空列表 = 清空）。
//!   本模块的 [`Simplify::react`] / [`Simplify::unreact`] 先 `getMe` 看
//!   `user.type.@type == "userTypeBot"`，自动挑对的那个。
//!
//! 请求一律经 [`HostApi::td_invoke_simplified`](fuyubot_abi::HostApi::td_invoke_simplified)
//! 发出，返回的仍是 TDLib 原始 JSON——本模块只简化输入。

use std::time::{Duration, Instant};

use fuyubot_abi::LogLevel;
use serde_json::{Map, Value, json};

use crate::{Result, Simplify, SimplifyError, Topic};

// ── 管理权 ────────────────────────────────────────────────────────

/// `chatAdministratorRights` 里的一个权限字段。
///
/// 名字就是 TDLib JSON 里的字段名（1.8.69 的全表）：`status.rights.<key>`。
/// 传 `None` = 只看「是不是管理员」；传 `Some(某项)` = 还要求那一项为 `true`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdminRight {
    /// 管理整个群（TDLib 里这一项关掉，其余权限再多也不算能管事）。
    ManageChat,
    ChangeInfo,
    PostMessages,
    EditMessages,
    DeleteMessages,
    InviteUsers,
    RestrictMembers,
    PinMessages,
    ManageTopics,
    PromoteMembers,
    ManageVideoChats,
    PostStories,
    EditStories,
    DeleteStories,
    ManageDirectMessages,
    ManageTags,
    SendWelcomeMessages,
    /// 以群的名义发言（匿名）。**群主**身上它是 `status.is_anonymous`，
    /// 不在 `rights` 里——[`is_admin_status`] 已经按这个差异处理了。
    IsAnonymous,
}

impl AdminRight {
    /// TDLib JSON 里的字段名。
    pub fn key(self) -> &'static str {
        match self {
            Self::ManageChat => "can_manage_chat",
            Self::ChangeInfo => "can_change_info",
            Self::PostMessages => "can_post_messages",
            Self::EditMessages => "can_edit_messages",
            Self::DeleteMessages => "can_delete_messages",
            Self::InviteUsers => "can_invite_users",
            Self::RestrictMembers => "can_restrict_members",
            Self::PinMessages => "can_pin_messages",
            Self::ManageTopics => "can_manage_topics",
            Self::PromoteMembers => "can_promote_members",
            Self::ManageVideoChats => "can_manage_video_chats",
            Self::PostStories => "can_post_stories",
            Self::EditStories => "can_edit_stories",
            Self::DeleteStories => "can_delete_stories",
            Self::ManageDirectMessages => "can_manage_direct_messages",
            Self::ManageTags => "can_manage_tags",
            Self::SendWelcomeMessages => "can_send_welcome_messages",
            Self::IsAnonymous => "is_anonymous",
        }
    }

    /// 反查字段名；认不出（比如更新的 TDLib 又加了一项）返回 `None`。
    pub fn from_key(key: &str) -> Option<Self> {
        [
            Self::ManageChat,
            Self::ChangeInfo,
            Self::PostMessages,
            Self::EditMessages,
            Self::DeleteMessages,
            Self::InviteUsers,
            Self::RestrictMembers,
            Self::PinMessages,
            Self::ManageTopics,
            Self::PromoteMembers,
            Self::ManageVideoChats,
            Self::PostStories,
            Self::EditStories,
            Self::DeleteStories,
            Self::ManageDirectMessages,
            Self::ManageTags,
            Self::SendWelcomeMessages,
            Self::IsAnonymous,
        ]
        .into_iter()
        .find(|right| right.key() == key)
    }

    /// 18 项全表——写配置映射、或者遍历检查时用。
    pub fn all() -> &'static [AdminRight] {
        &[
            Self::ManageChat,
            Self::ChangeInfo,
            Self::PostMessages,
            Self::EditMessages,
            Self::DeleteMessages,
            Self::InviteUsers,
            Self::RestrictMembers,
            Self::PinMessages,
            Self::ManageTopics,
            Self::PromoteMembers,
            Self::ManageVideoChats,
            Self::PostStories,
            Self::EditStories,
            Self::DeleteStories,
            Self::ManageDirectMessages,
            Self::ManageTags,
            Self::SendWelcomeMessages,
            Self::IsAnonymous,
        ]
    }
}

/// 一份 `ChatMemberStatus` 是不是管理员身份（`right` 为 `Some` 时还看那一项权限）。
///
/// 纯函数、不发请求：手上已经有一条 `ChatMember`（或 `updateChatMember` 的更新）时
/// 直接用它，省一次 `getChatMember` 往返。
///
/// - `chatMemberStatusCreator`：**群主拥有一切**，`right` 是什么都算有
///   （唯一的例外是 `IsAnonymous`——群主的匿名是 `status.is_anonymous`，不在 `rights` 里）；
/// - `chatMemberStatusAdministrator`：`None` → `true`；`Some(某项)` → `rights.<key>` 必须为 `true`，
///   字段缺失按 `false`；
/// - 其余（普通成员 / 受限 / 已退出 / 被封）→ `false`。
pub fn is_admin_status(status: &Value, right: Option<AdminRight>) -> bool {
    let kind = status.get("@type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "chatMemberStatusCreator" => match right {
            // 群主没有 rights 结构；「匿名」是 creator 自己的字段。
            Some(AdminRight::IsAnonymous) => status
                .get("is_anonymous")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            _ => true,
        },
        "chatMemberStatusAdministrator" => match right {
            None => true,
            Some(right) => status
                .pointer("/rights")
                .and_then(|rights| rights.get(right.key()))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        _ => false,
    }
}

/// 组装 `getChatMember` 的参数（`member_id` 是 `messageSenderUser`）。
pub fn chat_member_params(chat_id: i64, user_id: i64) -> Value {
    json!({
        "chat_id": chat_id,
        "member_id": { "@type": "messageSenderUser", "user_id": user_id },
    })
}

// ── 转发 ──────────────────────────────────────────────────────────

/// 转发一组消息的参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ForwardParams {
    /// 要转发的消息 id。**会被排序去重**（TDLib 要求严格递增）。
    pub message_ids: Vec<i64>,
    /// 转发到哪个话题；`None` = 不指定。
    pub topic_id: Option<Topic>,
    /// 不带原作者地复制内容（发到私密聊天时必为 `true`）。
    pub send_copy: bool,
    /// 去掉媒体标题（只在 `send_copy` 为真时有意义）。
    pub remove_caption: bool,
    /// 等响应的上限（毫秒）。
    pub timeout_ms: u64,
}

impl ForwardParams {
    pub fn new(message_ids: impl IntoIterator<Item = i64>) -> Self {
        Self {
            message_ids: message_ids.into_iter().collect(),
            ..Self::default()
        }
    }

    /// 转发到哪个话题（论坛话题是 [`Topic::forum`]）。
    pub fn topic_id(mut self, topic_id: Topic) -> Self {
        self.topic_id = Some(topic_id);
        self
    }

    pub fn send_copy(mut self, send_copy: bool) -> Self {
        self.send_copy = send_copy;
        self
    }

    pub fn remove_caption(mut self, remove_caption: bool) -> Self {
        self.remove_caption = remove_caption;
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
}

impl Default for ForwardParams {
    fn default() -> Self {
        Self {
            message_ids: Vec::new(),
            topic_id: None,
            send_copy: false,
            remove_caption: false,
            timeout_ms: crate::DEFAULT_TIMEOUT_MS,
        }
    }
}

/// 组装 `forwardMessages` 的参数。
///
/// `message_ids` 在这里**排序去重**：TDLib 要求严格递增，而调用方手里的 id 多半是从
/// 若干条更新里攒出来的，顺序没保证。去重会让「传入条数」和「返回条数」不一致——
/// 这是刻意的（TDLib 对重复 id 的态度是报错）。
///
/// `options`（`messageSendOptions`）不写：TDLib 允许省略，取默认。
pub fn forward_params(chat_id: i64, from_chat_id: i64, params: &ForwardParams) -> Value {
    let mut message_ids = params.message_ids.clone();
    message_ids.sort_unstable();
    message_ids.dedup();

    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert("from_chat_id".to_owned(), json!(from_chat_id));
    payload.insert("message_ids".to_owned(), json!(message_ids));
    // 话题的形状与 sendMessage 完全一致（同一个 put_common），不在这里另起一套。
    crate::put_common(&mut payload, &params.topic_id, &None);
    payload.insert("send_copy".to_owned(), json!(params.send_copy));
    payload.insert("remove_caption".to_owned(), json!(params.remove_caption));
    Value::Object(payload)
}

// ── 表情反应 ──────────────────────────────────────────────────────

/// 一个表情反应：普通 emoji 或自定义 emoji。
///
/// 付费反应（`reactionTypePaid`）不在这里——它走 `addPendingPaidMessageReaction`
/// 那一套，且 `setMessageReactions` 收不了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    /// `reactionTypeEmoji`：字符串本身就是一个 emoji（"👍"）。
    Emoji(String),
    /// `reactionTypeCustomEmoji`：自定义 emoji 的 id（不是字符串）。
    CustomEmoji(i64),
}

impl Reaction {
    pub fn emoji(emoji: impl Into<String>) -> Self {
        Self::Emoji(emoji.into())
    }

    pub fn custom_emoji(custom_emoji_id: i64) -> Self {
        Self::CustomEmoji(custom_emoji_id)
    }

    /// TDLib 的 `ReactionType`。
    pub fn to_json(&self) -> Value {
        match self {
            Self::Emoji(emoji) => json!({ "@type": "reactionTypeEmoji", "emoji": emoji }),
            Self::CustomEmoji(id) => {
                json!({ "@type": "reactionTypeCustomEmoji", "custom_emoji_id": id })
            }
        }
    }
}

impl From<&str> for Reaction {
    fn from(emoji: &str) -> Self {
        Self::emoji(emoji)
    }
}

impl From<String> for Reaction {
    fn from(emoji: String) -> Self {
        Self::emoji(emoji)
    }
}

/// 组装 `addMessageReaction` 的参数（**用户号**那条路）。
pub fn reaction_params(
    chat_id: i64,
    message_id: i64,
    reaction: &Reaction,
    is_big: bool,
    update_recent_reactions: bool,
) -> Value {
    json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction_type": reaction.to_json(),
        "is_big": is_big,
        "update_recent_reactions": update_recent_reactions,
    })
}

/// 组装 `setMessageReactions` 的参数（**bot 专用**那条路）。
///
/// 语义是「整组覆盖」：`reaction_types` 是什么，这条消息上就只剩什么；
/// 空列表 = 清空反应。
pub fn set_reactions_params(
    chat_id: i64,
    message_id: i64,
    reactions: &[Reaction],
    is_big: bool,
) -> Value {
    json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction_types": reactions.iter().map(Reaction::to_json).collect::<Vec<_>>(),
        "is_big": is_big,
    })
}

/// 组装 `removeMessageReaction` 的参数（**用户号**那条路）。
pub fn remove_reaction_params(chat_id: i64, message_id: i64, reaction: &Reaction) -> Value {
    json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction_type": reaction.to_json(),
    })
}

// ── Simplify 上的入口 ─────────────────────────────────────────────

impl Simplify {
    // ── 自己是谁 ──────────────────────────────────────────────

    /// `getMe`：自己的 `User`。超时 / 报错返回 `None`。
    ///
    /// ⚠️ **阻塞**当前线程（TDLib 的应答要经接收循环），回调里请用 `*_then` 那一套。
    pub fn me(&self, timeout_ms: u64) -> Option<Value> {
        self.ask(crate::td_api::GetMe::METHOD, "{}", timeout_ms)
    }

    /// 自己的 user id；拿不到（`getMe` 超时 / 报错）返回 `None`。
    pub fn my_id(&self, timeout_ms: u64) -> Option<i64> {
        self.me(timeout_ms)?.get("id").and_then(Value::as_i64)
    }

    /// 当前账号是不是 bot：看 `getMe` 的 `user.type.@type == "userTypeBot"`。
    ///
    /// TDLib 的 `user` **没有** `is_bot` 字段（那是 Bot API 的形状），身份在 `type` 里：
    /// `userTypeRegular` / `userTypeBot` / `userTypeDeleted` / `userTypeUnknown`。
    /// 拿不到 `getMe`（超时 / 报错）时按**用户号**处理——那也是 `addMessageReaction`
    /// 更常见的那一侧。
    pub fn is_bot(&self, timeout_ms: u64) -> bool {
        self.me(timeout_ms)
            .as_ref()
            .and_then(|me| me.pointer("/type/@type"))
            .and_then(Value::as_str)
            == Some("userTypeBot")
    }

    // ── 管理权 ────────────────────────────────────────────────

    /// `getChatMember`：某个成员在某个聊天里的 `ChatMember`（原始 JSON）。
    ///
    /// 超时、TDLib 报错（比如「成员不在群里」）都是 `None`——**不区分**这两种，
    /// 因为对「有没有管理权」这个问题来说它们答案一样。
    pub fn chat_member(&self, chat_id: i64, user_id: i64, timeout_ms: u64) -> Option<Value> {
        let params = chat_member_params(chat_id, user_id).to_string();
        let request = self
            .host()
            .td_invoke_simplified(crate::td_api::GetChatMember::METHOD, &params)
            .ok()?;
        let response = self.wait_response(request, timeout_ms)?;
        self.soften(&response, "getChatMember")
    }

    /// `user_id` 在 `chat_id` 里有没有管理权。
    ///
    /// - `right: None` → 只看身份（群主 / 管理员就算）；
    /// - `right: Some(AdminRight::DeleteMessages)` → 还得 `rights.can_delete_messages` 为真。
    ///
    /// 拿不到成员信息（超时 / TDLib 报错 / 不是群聊）一律 `false`——
    /// 「不确定」时说「没有」比说「有」安全。
    ///
    /// ⚠️ **阻塞**当前线程，回调里别用。
    pub fn is_chat_admin(
        &self,
        chat_id: i64,
        user_id: i64,
        right: Option<AdminRight>,
        timeout_ms: u64,
    ) -> bool {
        let Some(member) = self.chat_member(chat_id, user_id, timeout_ms) else {
            return false;
        };
        match member.get("status") {
            Some(status) => is_admin_status(status, right),
            None => false,
        }
    }

    /// **自己**在 `chat_id` 里有没有管理权（先 `getMe` 拿自己的 id，再查成员）。
    ///
    /// 「自己」在 TDLib 里没有捷径：`getChat` 不带「我是不是管理员」这个字段，
    /// 只能拿 `getMe` 的 id 去问 `getChatMember`。两次等待**各用一份** `timeout_ms`。
    ///
    /// ⚠️ **阻塞**当前线程，回调里别用。
    pub fn am_i_chat_admin(
        &self,
        chat_id: i64,
        right: Option<AdminRight>,
        timeout_ms: u64,
    ) -> bool {
        let Some(user_id) = self.my_id(timeout_ms) else {
            return false;
        };
        self.is_chat_admin(chat_id, user_id, right, timeout_ms)
    }

    // ── 转发 ──────────────────────────────────────────────────

    /// 把 `from_chat_id` 的一组消息转发到 `chat_id`，等到响应后返回那组消息。
    ///
    /// 返回的是 `Messages.messages`——**还没发完**的临时消息（和 `sendMessage` 的响应一样）。
    /// 最终消息照旧经 `updateMessageSendSucceeded` / `updateMessageSendFailed` 回来：
    /// 这里只等**响应**，不等发送结局（转发的成败通常不参与后续逻辑，多等一轮不划算）。
    /// 单项没转发成功时 TDLib 在对应位置放 `null`，所以数组长度和排序后的
    /// `message_ids` 一致、但元素可能是 `Null`。
    ///
    /// 校验：`message_ids` 不能为空、最多 100 条（TDLib 的硬上限）。
    ///
    /// ⚠️ **阻塞**当前线程，回调里请用 [`forward_messages_then`](Self::forward_messages_then)。
    pub fn forward_messages(
        &self,
        chat_id: i64,
        from_chat_id: i64,
        params: &ForwardParams,
    ) -> Result<Vec<Value>> {
        let count = params.message_ids.len();
        if count == 0 {
            return Err(SimplifyError::EmptyForward);
        }
        if count > MAX_FORWARD_MESSAGES {
            return Err(SimplifyError::TooManyForward(count));
        }

        let payload = forward_params(chat_id, from_chat_id, params).to_string();
        let request = self
            .host()
            .td_invoke_simplified(crate::td_api::ForwardMessages::METHOD, &payload)?;
        let response = self.await_response(
            request,
            Instant::now() + Duration::from_millis(params.timeout_ms),
            params.timeout_ms,
        )?;
        Ok(crate::pending_messages(&response))
    }

    /// [`forward_messages`](Self::forward_messages) 的异步版：结果从另一个线程交给 `on_done`。
    pub fn forward_messages_then(
        &self,
        chat_id: i64,
        from_chat_id: i64,
        params: &ForwardParams,
        on_done: impl FnOnce(Result<Vec<Value>>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        crate::spawn_then(
            move || simplify.forward_messages(chat_id, from_chat_id, &params),
            on_done,
        )
    }

    // ── 表情反应 ──────────────────────────────────────────────

    /// 给一条消息加一个反应，**自动挑方法**：
    ///
    /// - 用户号 → `addMessageReaction`（在已有反应上追加，`update_recent_reactions = true`）；
    /// - bot → `setMessageReactions`（**bot 只能用这个**，语义是「整组覆盖」，这里传单个）。
    ///
    /// 判定靠 `getMe` 的 `user.type`，也就是说每次调用会多一次 `getMe` 往返
    /// （它不碰服务器，TDLib 本地应答）。已经知道自己是 bot 就直接调
    /// [`set_reactions`](Self::set_reactions) / [`add_reaction`](Self::add_reaction)，省掉这一趟。
    ///
    /// ⚠️ **阻塞**当前线程，回调里请用 [`react_then`](Self::react_then)。
    pub fn react(
        &self,
        chat_id: i64,
        message_id: i64,
        reaction: &Reaction,
        is_big: bool,
        timeout_ms: u64,
    ) -> Result<()> {
        if self.is_bot(timeout_ms) {
            self.say(
                crate::td_api::SetMessageReactions::METHOD,
                &set_reactions_params(chat_id, message_id, std::slice::from_ref(reaction), is_big),
                timeout_ms,
            )
        } else {
            self.say(
                crate::td_api::AddMessageReaction::METHOD,
                &reaction_params(chat_id, message_id, reaction, is_big, true),
                timeout_ms,
            )
        }
    }

    /// 去掉一个反应，**自动挑方法**：
    ///
    /// - 用户号 → `removeMessageReaction`（去掉指定的那一个）；
    /// - bot → `setMessageReactions` 传空列表（bot 没有「删一个」的说法，只能整组覆盖；
    ///   空列表就是清空，所以 bot 侧**忽略** `reaction` 参数）。
    ///
    /// ⚠️ **阻塞**当前线程，回调里请用 [`unreact_then`](Self::unreact_then)。
    pub fn unreact(
        &self,
        chat_id: i64,
        message_id: i64,
        reaction: &Reaction,
        timeout_ms: u64,
    ) -> Result<()> {
        if self.is_bot(timeout_ms) {
            self.say(
                crate::td_api::SetMessageReactions::METHOD,
                &set_reactions_params(chat_id, message_id, &[], false),
                timeout_ms,
            )
        } else {
            self.say(
                crate::td_api::RemoveMessageReaction::METHOD,
                &remove_reaction_params(chat_id, message_id, reaction),
                timeout_ms,
            )
        }
    }

    /// 显式走**用户号**那条路（`addMessageReaction`），不问 `getMe`。
    pub fn add_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        reaction: &Reaction,
        is_big: bool,
        timeout_ms: u64,
    ) -> Result<()> {
        self.say(
            crate::td_api::AddMessageReaction::METHOD,
            &reaction_params(chat_id, message_id, reaction, is_big, true),
            timeout_ms,
        )
    }

    /// 显式走 **bot** 那条路（`setMessageReactions`），不问 `getMe`。
    ///
    /// `reactions` 为空 = 清空这条消息上的反应。
    pub fn set_reactions(
        &self,
        chat_id: i64,
        message_id: i64,
        reactions: &[Reaction],
        is_big: bool,
        timeout_ms: u64,
    ) -> Result<()> {
        self.say(
            crate::td_api::SetMessageReactions::METHOD,
            &set_reactions_params(chat_id, message_id, reactions, is_big),
            timeout_ms,
        )
    }

    /// [`react`](Self::react) 的异步版。
    pub fn react_then(
        &self,
        chat_id: i64,
        message_id: i64,
        reaction: Reaction,
        is_big: bool,
        timeout_ms: u64,
        on_done: impl FnOnce(Result<()>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        crate::spawn_then(
            move || simplify.react(chat_id, message_id, &reaction, is_big, timeout_ms),
            on_done,
        )
    }

    /// [`unreact`](Self::unreact) 的异步版。
    pub fn unreact_then(
        &self,
        chat_id: i64,
        message_id: i64,
        reaction: Reaction,
        timeout_ms: u64,
        on_done: impl FnOnce(Result<()>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        crate::spawn_then(
            move || simplify.unreact(chat_id, message_id, &reaction, timeout_ms),
            on_done,
        )
    }

    // ── 内部：发 + 等 ─────────────────────────────────────────

    /// 发一个只关心「成功 / 失败」的方法，等到响应。`Ok` / `error` 的判据同 `edit_*` 那套。
    fn say(&self, method: &str, payload: &Value, timeout_ms: u64) -> Result<()> {
        let request = self
            .host()
            .td_invoke_simplified(method, &payload.to_string())?;
        self.await_response(
            request,
            Instant::now() + Duration::from_millis(timeout_ms),
            timeout_ms,
        )
        .map(|_| ())
    }

    /// 发一个想知道返回值的方法，等到响应；报错 / 超时返回 `None`。
    ///
    /// 与 `say` 的区别只是**不把错误往上传**：身份类查询（我是谁 / 谁有管理权）
    /// 的调用方只想要「有 / 没有」，不该被迫处理 TDLib 的错误码。
    fn ask(&self, method: &str, params_json: &str, timeout_ms: u64) -> Option<Value> {
        let request = self.host().td_invoke_simplified(method, params_json).ok()?;
        let response = self.wait_response(request, timeout_ms)?;
        self.soften(&response, method)
    }

    /// 响应是 `error` 就记一行警告并当成「没拿到」，否则原样返回。
    fn soften(&self, response: &Value, method: &str) -> Option<Value> {
        match crate::error_of(response) {
            Some(error) => {
                self.host().log(
                    LogLevel::Warn,
                    &format!(
                        "{method} 被 TDLib 拒绝（{}）：{}",
                        error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        error.get("message").and_then(Value::as_str).unwrap_or("")
                    ),
                );
                None
            }
            None => Some(response.clone()),
        }
    }
}

/// `forwardMessages` 一次最多能带多少条（TDLib 的硬上限）。
pub const MAX_FORWARD_MESSAGES: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `chatAdministratorRights` 的 18 个字段（全 true）。
    fn all_rights() -> Value {
        let mut rights = Map::new();
        for right in AdminRight::all() {
            rights.insert(right.key().to_owned(), json!(true));
        }
        Value::Object(rights)
    }

    fn administrator() -> Value {
        json!({
            "@type": "chatMemberStatusAdministrator",
            "can_be_edited": true,
            "rights": all_rights(),
        })
    }

    #[test]
    fn only_creator_and_administrator_are_admins() {
        assert!(is_admin_status(&administrator(), None));
        assert!(is_admin_status(
            &json!({ "@type": "chatMemberStatusCreator", "is_anonymous": false, "is_member": true }),
            None
        ));
        for kind in [
            "chatMemberStatusMember",
            "chatMemberStatusRestricted",
            "chatMemberStatusLeft",
            "chatMemberStatusBanned",
        ] {
            assert!(
                !is_admin_status(&json!({ "@type": kind }), None),
                "{kind} 不该算管理员"
            );
        }
        // 空 / 认不出的形状也不算
        assert!(!is_admin_status(&Value::Null, None));
        assert!(!is_admin_status(
            &json!({ "@type": "chatMemberStatus" }),
            None
        ));
    }

    #[test]
    fn optional_rights_are_checked_field_by_field() {
        let mut rights = all_rights();
        rights["can_delete_messages"] = json!(false);
        let status = json!({
            "@type": "chatMemberStatusAdministrator",
            "can_be_edited": true,
            "rights": rights,
        });

        assert!(is_admin_status(&status, None), "没有指定权限时仍算管理员");
        assert!(
            is_admin_status(&status, Some(AdminRight::PinMessages)),
            "别的权限不受影响"
        );
        assert!(
            !is_admin_status(&status, Some(AdminRight::DeleteMessages)),
            "关掉的那一项要挡住"
        );
        // 字段缺失（比如老版本 TDLib 的响应）按 false
        let slim = json!({ "@type": "chatMemberStatusAdministrator", "rights": {} });
        assert!(!is_admin_status(&slim, Some(AdminRight::DeleteMessages)));
        assert!(is_admin_status(&slim, None));
    }

    #[test]
    fn creator_has_every_right_but_anonymity_is_its_own_field() {
        let anonymous = json!({
            "@type": "chatMemberStatusCreator",
            "is_anonymous": true,
            "is_member": true,
        });
        let plain = json!({
            "@type": "chatMemberStatusCreator",
            "is_anonymous": false,
            "is_member": true,
        });
        // 群主没有 rights 结构，其它权限一律算有
        assert!(is_admin_status(&plain, Some(AdminRight::DeleteMessages)));
        assert!(is_admin_status(&anonymous, Some(AdminRight::ManageChat)));
        // 只有「匿名」看 creator 自己的字段
        assert!(is_admin_status(&anonymous, Some(AdminRight::IsAnonymous)));
        assert!(!is_admin_status(&plain, Some(AdminRight::IsAnonymous)));
    }

    #[test]
    fn admin_rights_round_trip_through_their_tdlib_key() {
        for right in AdminRight::all() {
            assert_eq!(AdminRight::from_key(right.key()), Some(*right));
        }
        assert_eq!(AdminRight::all().len(), 18);
        assert_eq!(AdminRight::from_key("is_bot"), None, "Bot API 的字段不认");
    }

    #[test]
    fn reactions_become_one_reaction_type_each() {
        assert_eq!(
            Reaction::emoji("👍").to_json(),
            json!({ "@type": "reactionTypeEmoji", "emoji": "👍" })
        );
        assert_eq!(
            Reaction::custom_emoji(42).to_json(),
            json!({ "@type": "reactionTypeCustomEmoji", "custom_emoji_id": 42 })
        );
        // 从 &str / String 直接转，省掉一层
        assert_eq!(Reaction::from("🎉"), Reaction::emoji("🎉"));
        assert_eq!(Reaction::from("x".to_owned()), Reaction::emoji("x"));
    }

    #[test]
    fn forward_params_sort_and_dedup_message_ids() {
        let params = ForwardParams::new([9, 3, 9, 1])
            .topic_id(Topic::forum(5))
            .send_copy(true);
        let payload = forward_params(7, 8, &params);
        assert_eq!(payload["chat_id"], 7);
        assert_eq!(payload["from_chat_id"], 8);
        assert_eq!(payload["message_ids"], json!([1, 3, 9]), "严格递增且去重");
        assert_eq!(payload["topic_id"]["@type"], "messageTopicForum");
        assert_eq!(payload["topic_id"]["forum_topic_id"], 5);
        assert_eq!(payload["send_copy"], true);
        assert_eq!(payload["remove_caption"], false);
        assert!(
            payload.get("options").is_none(),
            "省略 options 让 TDLib 取默认"
        );

        // 不带话题时干脆不留这个键
        let bare = forward_params(7, 8, &ForwardParams::new([1]));
        assert!(bare.get("topic_id").is_none());
    }

    /// 四种 `MessageTopic` 各有各的构造器与 id 类型，选错 TDLib 就拒。
    #[test]
    fn every_message_topic_constructor_is_reachable() {
        let cases = [
            (Topic::forum(1), "messageTopicForum", "forum_topic_id"),
            (Topic::thread(2), "messageTopicThread", "message_thread_id"),
            (
                Topic::direct_messages(3),
                "messageTopicDirectMessages",
                "direct_messages_chat_topic_id",
            ),
            (
                Topic::saved_messages(4),
                "messageTopicSavedMessages",
                "saved_messages_topic_id",
            ),
        ];
        for (topic, kind, field) in cases {
            let json = topic.to_json();
            assert_eq!(json["@type"], kind);
            assert!(json.get(field).is_some(), "{kind} 应该带 {field}");
            // 四个构造器都在真 schema 里（拼错的话 TDLib 认不出这个 topic_id）
            let entry = fuyubot_tdlib_sys::TdApiSchema::compiled()
                .get(kind)
                .unwrap_or_else(|| panic!("真 tl 里应该有 {kind}"));
            assert!(entry.has_param(field), "{kind} 的参数名应该是 {field}");
        }
    }

    /// 新拼的每种参数都要过**真** `td_api.tl` 烤出来的元数据。
    ///
    /// 方法名 / 参数名拼错在本地看不出来（请求照样发出去），只有 TDLib 会拒——
    /// 所以在这里挡住（`every_request_simplify_builds_passes_the_real_schema` 覆盖
    /// 的是**发得出去**的那批，这里补上 bot 分支那种「要看账号才走得到」的）。
    #[test]
    fn new_request_shapes_pass_the_real_schema() {
        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        let reactions = [Reaction::emoji("👍"), Reaction::custom_emoji(1)];
        let cases: Vec<(&str, Value)> = vec![
            ("getChatMember", chat_member_params(7, 42)),
            (
                "forwardMessages",
                forward_params(7, 8, &ForwardParams::new([1, 2])),
            ),
            (
                crate::td_api::AddMessageReaction::METHOD,
                reaction_params(7, 9, &Reaction::emoji("👍"), false, true),
            ),
            (
                crate::td_api::SetMessageReactions::METHOD,
                set_reactions_params(7, 9, &reactions, true),
            ),
            // bot 的「清空」：空列表也是合法形状
            (
                crate::td_api::SetMessageReactions::METHOD,
                set_reactions_params(7, 9, &[], false),
            ),
            (
                "removeMessageReaction",
                remove_reaction_params(7, 9, &Reaction::emoji("👍")),
            ),
        ];
        for (method, params) in cases {
            schema
                .check_request(method, &params)
                .unwrap_or_else(|error| panic!("{method} 拼错了：{error}\n{params}"));
        }
    }
}
