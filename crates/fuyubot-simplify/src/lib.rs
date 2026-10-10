//! TDLib 便捷封装：**只简化输入，不改变输出**。
//!
//! 全部方法都经 [`HostApi::td_invoke`] / [`HostApi::td_invoke_simplified`] 发出，
//! 不绕过 ABI、也不碰 TDLib 的库；原始更新照旧全部对插件可见（这里只**额外**推简化事件）。
//! 想用什么就用什么——封装是可选糖，不是黑盒。
//!
//! ## 两条必须知道的时间线
//!
//! - `td_invoke*` 返回的是**请求号**，真正的 JSON 从响应通道回来（事件类型
//!   [`RESPONSE_EVENT`](fuyubot_abi::RESPONSE_EVENT)，只发给发起请求的那个插件）。
//! - [`WaitHandle::payload_json`](fuyubot_abi::WaitHandle) 是宿主临时的线程局部缓冲，
//!   **只在下一次宿主调用之前有效**。要拿数据就立刻读，或者用本文的 `*_json` 系列
//!   （它们已经替你拷好了）。

use std::ffi::CStr;
use std::time::{Duration, Instant};

use fuyubot_abi::{
    FILTER_EVENT_TYPE_KEY, HostApi, RESPONSE_EVENT, RequestId, UPDATE_EVENT, WaitHandle,
};
use serde_json::{Map, Value, json};

/// 封装层的错误。
#[derive(Debug, thiserror::Error)]
pub enum SimplifyError {
    #[error(transparent)]
    Abi(#[from] fuyubot_abi::AbiError),
    #[error("参数串里含 NUL 字节")]
    BadString,
}

type Result<T> = std::result::Result<T, SimplifyError>;

/// 账号类型。
///
/// TDLib 里**发送只有一个方法 `sendMessage`**（Bot 与 User 都是它）：内容类型靠
/// `input_message_content` 区分。`sendPhoto` / `sendDocument` / `sendLocation` 是
/// Telegram **Bot API**（HTTP 接口）的方法名，**TDLib 里不存在**——真 `td_api.tl` 里
/// grep 得到，详见 [`method_for`]。
///
/// 这个参数保留在签名里，是因为 TDLib 确实有只对其中一种账号有效的方法，
/// 而那种差异应该只在一个地方编码。当前两种账号取值相同，测试把这一点显式断言下来，
/// 将来 TDLib 分化时改一处即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountType {
    Bot,
    User,
}

/// 文本解析模式。
///
/// ⚠️ **这一版不会被发出去**：TDLib 的 `formattedText` 只有 `text` + `entities`，
/// **没有 `parse_mode`**（那是 Bot API 的概念）。带标记的文本在 TDLib 里要先经
/// `parseTextEntities` 换成 entities，见 [`send_params`] 上的 `ponytail:` 注释。
/// 字段留着是为了公共 API 稳定，值暂时按纯文本处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseMode {
    #[default]
    Plain,
    Markdown,
    MarkdownV2,
    Html,
}

/// 要发的内容。
#[derive(Debug, Clone)]
pub enum MessageContent {
    Text {
        text: String,
        parse_mode: ParseMode,
    },
    Photo {
        path: String,
        caption: Option<String>,
    },
    Document {
        path: String,
        caption: Option<String>,
    },
    Location {
        latitude: f64,
        longitude: f64,
    },
}

impl MessageContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            parse_mode: ParseMode::Plain,
        }
    }

    pub fn photo(path: impl Into<String>) -> Self {
        Self::Photo {
            path: path.into(),
            caption: None,
        }
    }

    pub fn document(path: impl Into<String>) -> Self {
        Self::Document {
            path: path.into(),
            caption: None,
        }
    }

    pub fn location(latitude: f64, longitude: f64) -> Self {
        Self::Location {
            latitude,
            longitude,
        }
    }
}

/// 发送选项。
#[derive(Debug, Clone)]
pub struct SendOptions {
    pub account_type: AccountType,
    pub message_thread_id: Option<i64>,
    pub reply_to_message_id: Option<i64>,
    pub disable_notification: bool,
    pub protect_content: bool,
    /// 直接塞进 `replyMarkup`（内联键盘用 [`inline_keyboard`] 生成）。
    pub reply_markup: Option<Value>,
    /// `send_message_wait_success` 的等待上限。
    pub timeout_ms: u64,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            account_type: AccountType::Bot,
            message_thread_id: None,
            reply_to_message_id: None,
            disable_notification: false,
            protect_content: false,
            reply_markup: None,
            timeout_ms: 30_000,
        }
    }
}

/// 内联键盘按钮。
#[derive(Debug, Clone)]
pub enum Button {
    /// 点下去回一条 callback query 给机器人。
    Callback { text: String, data: String },
    /// 打开链接（用户账号不能发 URL 按钮，TDLib 会拒绝）。
    Url { text: String, url: String },
}

impl Button {
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Callback {
            text: text.into(),
            data: data.into(),
        }
    }

    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self::Url {
            text: text.into(),
            url: url.into(),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Callback { text, data } => json!({
                "@type": "inlineKeyboardButton",
                "text": text,
                "type": { "@type": "inlineKeyboardButtonTypeCallback", "data": base64(data) },
            }),
            Self::Url { text, url } => json!({
                "@type": "inlineKeyboardButton",
                "text": text,
                "type": { "@type": "inlineKeyboardButtonTypeUrl", "url": url },
            }),
        }
    }
}

/// TDLib 的 callback data 是 **base64** 编码的字节串。
fn base64(data: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 构造内联键盘的 `replyMarkup`。
pub fn inline_keyboard(rows: Vec<Vec<Button>>) -> Value {
    json!({
        "@type": "replyMarkupInlineKeyboard",
        "rows": rows
            .iter()
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|button| {
                            let mut object = button.to_json();
                            object["@type"] = json!("inlineKeyboardButton");
                            object
                        })
                        .collect(),
                )
            })
            .collect::<Vec<_>>(),
    })
}

/// 要解析的实体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityId {
    Chat(i64),
    User(i64),
}

impl EntityId {
    fn method(&self) -> &'static str {
        match self {
            Self::Chat(_) => "getChat",
            Self::User(_) => "getUser",
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::Chat(id) => json!({ "chat_id": id }),
            Self::User(id) => json!({ "user_id": id }),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Chat(id) => json!({ "kind": "chat", "id": id }),
            Self::User(id) => json!({ "kind": "user", "id": id }),
        }
    }
}

/// 等更新的过滤器。
///
/// TDLib 的字段位置随更新类型变：`updateNewMessage` 这类把消息包在 `message` 里
/// （所以 `chat_id` 是 `message.chat_id`），而 `updateNewCallbackQuery` 这类把
/// `chat_id` 放在顶层。用 [`UpdateFilter::flat`] 切换后者。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateFilter {
    update_type: Option<String>,
    chat_id: Option<i64>,
    sender_user_id: Option<i64>,
    message_thread_id: Option<i64>,
    flat: bool,
}

impl UpdateFilter {
    /// 按「消息包在 `message` 里」的更新类型过滤，例如 `updateNewMessage`。
    pub fn new(update_type: impl Into<String>) -> Self {
        Self {
            update_type: Some(update_type.into()),
            chat_id: None,
            sender_user_id: None,
            message_thread_id: None,
            flat: false,
        }
    }

    /// 字段在顶层的更新类型，例如 `updateNewCallbackQuery`。
    pub fn flat(update_type: impl Into<String>) -> Self {
        Self {
            flat: true,
            ..Self::new(update_type)
        }
    }

    /// 不限定类型（走兜底通道），只按下面的字段匹配。
    pub fn any() -> Self {
        Self {
            update_type: None,
            chat_id: None,
            sender_user_id: None,
            message_thread_id: None,
            flat: false,
        }
    }

    pub fn chat_id(mut self, chat_id: i64) -> Self {
        self.chat_id = Some(chat_id);
        self
    }

    pub fn sender_user_id(mut self, sender_user_id: i64) -> Self {
        self.sender_user_id = Some(sender_user_id);
        self
    }

    pub fn message_thread_id(mut self, message_thread_id: i64) -> Self {
        self.message_thread_id = Some(message_thread_id);
        self
    }

    /// 转成宿主认的过滤器 JSON。
    ///
    /// `update_type` 走的是**通道选择**（`event_type` 键），不是负载字段匹配：
    /// 宿主把每条更新同时发到具体 `@type` 与兜底 `"update"` 两条通道上。
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert(
            FILTER_EVENT_TYPE_KEY.to_owned(),
            json!(self.update_type.as_deref().unwrap_or(UPDATE_EVENT)),
        );

        let prefix = if self.flat { "" } else { "message." };
        let mut put = |key: &str, value: Option<i64>| {
            if let Some(value) = value {
                object.insert(format!("{prefix}{key}"), json!(value));
            }
        };
        put("chat_id", self.chat_id);
        put("sender_user_id", self.sender_user_id);
        put("message_thread_id", self.message_thread_id);
        Value::Object(object)
    }
}

/// 一次实体解析的结果。
#[derive(Debug, Clone)]
pub struct EntitiesResolution {
    /// 聚合关联 id：本次最后一条发出的请求（只为日志关联）。
    pub request_id: RequestId,
    pub entities: Vec<EntityResult>,
}

impl EntitiesResolution {
    pub fn to_json(&self) -> Value {
        json!({
            "request_id": self.request_id.id,
            "entities": self.entities.iter().map(EntityResult::to_json).collect::<Vec<_>>(),
        })
    }
}

/// 一个实体的解析结果。
#[derive(Debug, Clone)]
pub struct EntityResult {
    pub id: EntityId,
    pub request: RequestId,
    /// TDLib 原始 JSON；超时或没等到时是 `Null`。
    pub value: Value,
}

impl EntityResult {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id.to_json(),
            "request_id": self.request.id,
            "value": self.value,
        })
    }
}

/// 下载结束后的状态。
#[derive(Debug, Clone)]
pub struct FileDownload {
    pub request_id: RequestId,
    pub file_id: i32,
    pub completed: bool,
    /// 最后一次 `updateFile` 的原始 JSON。
    pub last_update: Value,
}

/// 发送成功的确认。
#[derive(Debug, Clone)]
pub struct SendConfirmation {
    /// 发起请求时拿到的请求号（对应最初的临时消息）。
    pub request_id: RequestId,
    /// 等到 `updateMessageSendSucceeded` 了没有。
    pub confirmed: bool,
    /// 那条更新的原始 JSON。
    pub update: Value,
}

/// `(账号类型, 内容)` → TDLib 方法名。
///
/// **TDLib 只有 `sendMessage`**：内容类型放 `input_message_content`。它没有
/// `sendPhoto` / `sendDocument` / `sendLocation`——那三个是 Telegram **Bot API**
/// （HTTP 接口）的方法名，TDLib 里根本不存在。这是真 `td_api.tl` 验出来的事实，
/// 也是 `every_request_simplify_builds_passes_the_real_schema` 抓到过的错误。
///
/// 保留 `account` 参数是因为 TDLib 确实有只对某一种账号有效的方法，那种差异应该在
/// 这一处编码；当前两种账号取值相同，测试显式钉住。
pub fn method_for(_account: AccountType, _content: &MessageContent) -> &'static str {
    "sendMessage"
}

/// 内容 → `InputMessageContent`（`sendMessage` 的 `input_message_content` 字段）。
///
/// 形状全部照真 `td_api.tl`：
/// - `inputMessageText text:formattedText`
/// - `inputMessagePhoto photo:inputPhoto caption:formattedText`，而
///   `inputPhoto photo:InputFile` —— **`photo` 套了两层**，内层那个才是文件。
/// - `inputMessageDocument document:inputDocument caption:formattedText`，`inputDocument document:InputFile`
/// - `inputMessageLocation location:location`
///
/// **caption 在内容类型里，不在 `sendMessage` 的顶层**（顶层没有 `caption` 这个参数）。
pub fn content_json(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text { text, .. } => json!({
            "@type": "inputMessageText",
            "text": { "@type": "formattedText", "text": text, "entities": [] },
        }),
        MessageContent::Photo { path, caption } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessagePhoto"));
            object.insert(
                "photo".to_owned(),
                json!({
                    "@type": "inputPhoto",
                    "photo": { "@type": "inputFileLocal", "path": path },
                }),
            );
            if let Some(caption) = caption {
                object.insert("caption".to_owned(), caption_json(caption));
            }
            Value::Object(object)
        }
        MessageContent::Document { path, caption } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageDocument"));
            object.insert(
                "document".to_owned(),
                json!({
                    "@type": "inputDocument",
                    "document": { "@type": "inputFileLocal", "path": path },
                }),
            );
            if let Some(caption) = caption {
                object.insert("caption".to_owned(), caption_json(caption));
            }
            Value::Object(object)
        }
        MessageContent::Location {
            latitude,
            longitude,
        } => json!({
            "@type": "inputMessageLocation",
            "location": {
                "@type": "location",
                "latitude": latitude,
                "longitude": longitude,
            },
        }),
    }
}

/// 组装发送请求的参数（不含 `@type`，那是 [`HostApi::td_invoke_simplified`] 补的）。
///
/// 参数名以真 `td_api.tl` 为准（`sendMessage chat_id topic_id reply_to options
/// reply_markup input_message_content`）：
/// - 楼层是 **`topic_id`**（`messageTopicThread`），不是 `message_thread_id`；
/// - 回复是 **`reply_to`**（`inputMessageReplyToMessage`），不是 `reply_to_message_id`；
/// - `disable_notification` / `protect_content` 装在 **`options`**（`messageSendOptions`）
///   里，**不是顶层参数**；
/// - **没有顶层 `caption`**，它在内容类型里（见 [`content_json`]）。
///
/// 这一版修掉的是一次真缺陷：上面几条以前全按 **Bot API** 的样子拼，
/// 除了 `chat_id` / `reply_markup` / `input_message_content`，参数名 TDLib 一个都不认。
/// `every_request_simplify_builds_passes_the_real_schema` 会挡住再犯。
// ponytail: `parse_mode` 暂时被忽略（带标记的文本按纯文本发）。要真支持就得走
// `parseTextEntities`（text + parse_mode → formattedText 的 entities）再塞进
// `inputMessageText.text`——那需要一次 td_invoke 往返，不能在这个纯函数里做。
pub fn send_params(chat_id: i64, content: &MessageContent, options: &SendOptions) -> Value {
    let mut params = Map::new();
    params.insert("chat_id".to_owned(), json!(chat_id));

    if let Some(thread) = options.message_thread_id {
        params.insert(
            "topic_id".to_owned(),
            json!({ "@type": "messageTopicThread", "message_thread_id": thread }),
        );
    }
    if let Some(message) = options.reply_to_message_id {
        params.insert(
            "reply_to".to_owned(),
            json!({ "@type": "inputMessageReplyToMessage", "message_id": message }),
        );
    }

    if options.disable_notification || options.protect_content {
        let mut send_options = Map::new();
        send_options.insert("@type".to_owned(), json!("messageSendOptions"));
        if options.disable_notification {
            send_options.insert("disable_notification".to_owned(), json!(true));
        }
        if options.protect_content {
            send_options.insert("protect_content".to_owned(), json!(true));
        }
        params.insert("options".to_owned(), Value::Object(send_options));
    }

    if let Some(markup) = &options.reply_markup {
        params.insert("reply_markup".to_owned(), markup.clone());
    }

    params.insert("input_message_content".to_owned(), content_json(content));
    Value::Object(params)
}

fn caption_json(caption: &str) -> Value {
    json!({ "@type": "formattedText", "text": caption, "entities": [] })
}

/// TDLib 便捷入口。持有一个 [`HostApi`]（它是 `Copy` 的，随手复制）。
#[derive(Debug, Clone, Copy)]
pub struct Simplify {
    host: HostApi,
}

impl Simplify {
    pub fn new(host: HostApi) -> Self {
        Self { host }
    }

    pub fn host(&self) -> HostApi {
        self.host
    }

    // ── A. 发送 ────────────────────────────────────────────────────

    /// 发消息。返回**请求号**；TDLib 的临时消息 JSON 会从响应通道回来。
    pub fn send_message(
        &self,
        chat_id: i64,
        content: MessageContent,
        options: &SendOptions,
    ) -> Result<RequestId> {
        let method = method_for(options.account_type, &content);
        let params = send_params(chat_id, &content, options).to_string();
        Ok(self.host.td_invoke_simplified(method, &params)?)
    }

    /// 发消息并**阻塞**等到 `updateMessageSendSucceeded`。
    pub fn send_message_wait_success(
        &self,
        chat_id: i64,
        content: MessageContent,
        options: &SendOptions,
    ) -> Result<SendConfirmation> {
        let request_id = self.send_message(chat_id, content, options)?;
        let filter = UpdateFilter::new("updateMessageSendSucceeded")
            .chat_id(chat_id)
            .to_json()
            .to_string();
        let handle = self.host.wait_for_update(&filter, options.timeout_ms);
        let update = payload_of(handle).unwrap_or(Value::Null);
        Ok(SendConfirmation {
            request_id,
            confirmed: handle.did_match() && !update.is_null(),
            update,
        })
    }

    /// 发一条带内联键盘的消息。
    pub fn send_message_with_keyboard(
        &self,
        chat_id: i64,
        text: impl Into<String>,
        buttons: Vec<Vec<Button>>,
    ) -> Result<RequestId> {
        let options = SendOptions {
            reply_markup: Some(inline_keyboard(buttons)),
            ..SendOptions::default()
        };
        self.send_message(chat_id, MessageContent::text(text), &options)
    }

    /// 回一条 callback query（机器人点了内联按钮之后必须回，否则客户端一直转圈）。
    pub fn answer_callback_query(
        &self,
        callback_query_id: i64,
        text: Option<&str>,
        show_alert: bool,
    ) -> Result<RequestId> {
        let params = json!({
            "callback_query_id": callback_query_id,
            "text": text.unwrap_or_default(),
            "show_alert": show_alert,
            "url": "",
            "cache_time": 0,
        });
        Ok(self
            .host
            .td_invoke_simplified("answerCallbackQuery", &params.to_string())?)
    }

    // ── B. 等更新 ──────────────────────────────────────────────────

    /// 等一条匹配的更新。**阻塞**当前线程，直到匹配、超时或被 `cancel_wait` 取消。
    ///
    /// ⚠️ 返回的 [`WaitHandle::payload_json`] 是宿主临时缓冲，下一次宿主调用前有效；
    /// 想省心就用 [`wait_for_update_json`](Self::wait_for_update_json)。
    pub fn wait_for_update(&self, filter: &UpdateFilter, timeout_ms: u64) -> WaitHandle {
        self.host
            .wait_for_update(&filter.to_json().to_string(), timeout_ms)
    }

    /// 同上，但顺手把负载拷成 [`Value`]。超时返回 `None`。
    pub fn wait_for_update_json(
        &self,
        filter: &UpdateFilter,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let filter = filter.to_json().to_string();
        let handle = self.host.wait_for_update(&filter, timeout_ms);
        Ok(payload_of(handle))
    }

    /// 取消一个还没结束的等待（从另一个线程发起）。
    pub fn cancel_wait(&self, handle: WaitHandle) {
        self.host.cancel_wait(handle);
    }

    // ── D. 批量实体解析 ────────────────────────────────────────────

    /// 并发发起 `getChat` / `getUser`，逐个收齐后聚合成一个事件推回去。
    ///
    /// **会阻塞**：请求是同时发出的，收集是逐个等的，每个的等待上限都是 `timeout_ms`。
    /// 聚合结果同时以 `"entities_resolved"` 事件推送（负载见 [`EntitiesResolution::to_json`]）。
    pub fn resolve_entities(
        &self,
        ids: Vec<EntityId>,
        timeout_ms: u64,
    ) -> Result<EntitiesResolution> {
        // 1. 先把请求全发出去（这一步是并发的：不等任何响应）
        let mut requests = Vec::with_capacity(ids.len());
        for id in &ids {
            let params = id.params().to_string();
            requests.push(self.host.td_invoke_simplified(id.method(), &params)?);
        }

        // 2. 逐个等响应
        let mut entities = Vec::with_capacity(ids.len());
        for (id, request) in ids.into_iter().zip(&requests) {
            let filter = json!({
                FILTER_EVENT_TYPE_KEY: RESPONSE_EVENT,
                "request_id": request.id,
            })
            .to_string();
            let handle = self.host.wait_for_update(&filter, timeout_ms);
            entities.push(EntityResult {
                id,
                request: *request,
                value: payload_of(handle).unwrap_or(Value::Null),
            });
        }

        let resolution = EntitiesResolution {
            request_id: requests.last().copied().unwrap_or(RequestId::INVALID),
            entities,
        };
        // 3. 聚合成一个事件推回去（失败也不影响返回值）
        let _ = self
            .host
            .emit("entities_resolved", &resolution.to_json().to_string());
        Ok(resolution)
    }

    // ── E. 文件下载进度 ────────────────────────────────────────────

    /// 发起下载并**阻塞**到下载完成（或超时），期间每收到一条 `updateFile` 就推一个
    /// 简化事件 `"file_progress"`。
    ///
    /// 想看实时进度又不想阻塞，就直接订阅原始的 `updateFile`——原始更新一直可见。
    pub fn download_file(
        &self,
        file_id: i32,
        priority: i32,
        timeout_ms: u64,
    ) -> Result<FileDownload> {
        let params = json!({ "file_id": file_id, "priority": priority, "offset": 0, "limit": 0 });
        let request_id = self
            .host
            .td_invoke_simplified("downloadFile", &params.to_string())?;

        // TDLib 的 updateFile 长这样：{"@type":"updateFile","file":{"id":..,"local":{..}}}
        // —— file id 在 `file.id`，不是顶层。
        let filter = json!({ FILTER_EVENT_TYPE_KEY: UPDATE_EVENT, "file.id": file_id }).to_string();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last_update = Value::Null;
        let mut completed = false;

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let handle = self
                .host
                .wait_for_update(&filter, remaining.as_millis() as u64);
            let Some(update) = payload_of(handle) else {
                break; // 超时或被取消
            };

            completed = update
                .pointer("/file/local/is_downloading_completed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.emit_progress(file_id, &update, completed);
            last_update = update;
            if completed {
                break;
            }
        }

        Ok(FileDownload {
            request_id,
            file_id,
            completed,
            last_update,
        })
    }

    fn emit_progress(&self, file_id: i32, update: &Value, completed: bool) {
        let simplified = json!({
            "file_id": file_id,
            "completed": completed,
            "downloaded_size": update.pointer("/file/local/downloaded_size"),
            "expected_size": update.pointer("/file/expected_size"),
        });
        let _ = self.host.emit("file_progress", &simplified.to_string());
    }
}

/// 把 [`WaitHandle`] 的负载立刻拷成 [`Value`]；没匹配上就是 `None`。
///
/// 必须在**任何**其它宿主调用之前调用它——宿主返回的字符串是线程局部缓冲。
fn payload_of(handle: WaitHandle) -> Option<Value> {
    if !handle.did_match() || handle.payload_json.is_null() {
        return None;
    }
    // SAFETY: 宿主保证这个指针在下一次宿主调用之前有效，这里立刻读完。
    let text = unsafe { CStr::from_ptr(handle.payload_json) }.to_string_lossy();
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_abi::{HostApi, UPDATE_EVENT as ABI_UPDATE_EVENT};
    use fuyubot_core::Context;
    use fuyubot_loader::HostRuntime;
    use std::sync::Arc;

    fn runtime() -> Arc<HostRuntime> {
        HostRuntime::new(Context::new()).expect("运行时应该建得起来")
    }

    fn simplify(rt: &Arc<HostRuntime>) -> Simplify {
        // SAFETY: rt 比 simplify 活得久。
        let host = unsafe { HostApi::from_raw(rt.vtable_ptr()) }.unwrap();
        Simplify::new(host)
    }

    /// 验收 1：Bot 模式生成 `sendMessage`，User 模式生成正确方法。
    #[test]
    fn send_message_picks_the_right_method_per_account() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let bot = SendOptions::default();
        let user = SendOptions {
            account_type: AccountType::User,
            ..SendOptions::default()
        };

        simplify
            .send_message(7, MessageContent::text("hi"), &bot)
            .unwrap();
        simplify
            .send_message(7, MessageContent::text("hi"), &user)
            .unwrap();
        simplify
            .send_message(7, MessageContent::photo("/tmp/a.jpg"), &user)
            .unwrap();
        simplify
            .send_message(7, MessageContent::document("/tmp/a.pdf"), &user)
            .unwrap();
        simplify
            .send_message(7, MessageContent::location(1.5, 2.5), &bot)
            .unwrap();

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|r| r.method.as_str()).collect();
        // 五种内容**全是 sendMessage**：TDLib 没有 sendPhoto / sendDocument / sendLocation
        // （那三个是 Bot API 的方法名，见 `method_for`）。以前这里钉的是错的行为。
        assert_eq!(
            methods,
            [
                "sendMessage",
                "sendMessage",
                "sendMessage",
                "sendMessage",
                "sendMessage"
            ]
        );

        // 参数拼装：input_message_content + chat_id
        let params = &sent[0].params;
        assert_eq!(params["chat_id"], 7);
        assert_eq!(params["input_message_content"]["@type"], "inputMessageText");
        assert_eq!(params["input_message_content"]["text"]["text"], "hi");
        // 位置走 inputMessageLocation，经纬度嵌在 location 里（不是顶层）
        assert_eq!(
            sent[4].params["input_message_content"]["@type"],
            "inputMessageLocation"
        );
        assert_eq!(
            sent[4].params["input_message_content"]["location"]["latitude"],
            1.5
        );
        // 图片的 photo 套了两层：inputMessagePhoto.photo(inputPhoto).photo(InputFile)
        assert_eq!(
            sent[2].params["input_message_content"]["photo"]["@type"],
            "inputPhoto"
        );
        assert_eq!(
            sent[2].params["input_message_content"]["photo"]["photo"]["@type"],
            "inputFileLocal"
        );
        // 文档同理：inputMessageDocument.document(inputDocument).document(InputFile)
        assert_eq!(
            sent[3].params["input_message_content"]["document"]["document"]["@type"],
            "inputFileLocal"
        );
    }

    #[test]
    fn send_options_are_carried_into_the_request() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let options = SendOptions {
            message_thread_id: Some(3),
            reply_to_message_id: Some(9),
            disable_notification: true,
            protect_content: true,
            reply_markup: Some(inline_keyboard(vec![vec![Button::callback("ok", "x")]])),
            ..SendOptions::default()
        };
        simplify
            .send_message(7, MessageContent::text("hi"), &options)
            .unwrap();

        let params = rt.sent_requests()[0].params.clone();
        // 真 `sendMessage` 的签名是 `chat_id topic_id reply_to options reply_markup
        // input_message_content`——楼层/回复/选项都不是顶层参数（以前拼错了）。
        assert_eq!(params["topic_id"]["@type"], "messageTopicThread");
        assert_eq!(params["topic_id"]["message_thread_id"], 3);
        assert_eq!(params["reply_to"]["@type"], "inputMessageReplyToMessage");
        assert_eq!(params["reply_to"]["message_id"], 9);
        assert_eq!(params["options"]["@type"], "messageSendOptions");
        assert_eq!(params["options"]["disable_notification"], true);
        assert_eq!(params["options"]["protect_content"], true);
        assert_eq!(params["reply_markup"]["@type"], "replyMarkupInlineKeyboard");
        assert_eq!(
            params["reply_markup"]["rows"][0][0]["type"]["@type"],
            "inlineKeyboardButtonTypeCallback"
        );
    }

    /// 验收 2：wait_for_update 超时返回未匹配；匹配时拿到负载。
    #[test]
    fn wait_for_update_reports_timeout_and_match() {
        let rt = runtime();
        let simplify = simplify(&rt);

        // 超时：没人发更新
        let missed = simplify.wait_for_update_json(&UpdateFilter::new("updateNewMessage"), 40);
        assert!(missed.unwrap().is_none(), "没人发更新就该超时");

        // 匹配：另一个线程发一条
        let publisher = Arc::clone(&rt);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            publisher
                .publish_update(&json!({"@type":"updateNewMessage","message":{"id":5}}))
                .unwrap();
        });
        let got = simplify
            .wait_for_update_json(&UpdateFilter::new("updateNewMessage"), 2_000)
            .unwrap();
        handle.join().unwrap();

        let got = got.expect("应该等到那条更新");
        assert_eq!(got["message"]["id"], 5);
    }

    #[test]
    fn update_filter_paths_depend_on_the_update_shape() {
        let wrapped = UpdateFilter::new("updateNewMessage")
            .chat_id(42)
            .sender_user_id(7)
            .message_thread_id(3)
            .to_json();
        assert_eq!(wrapped[FILTER_EVENT_TYPE_KEY], "updateNewMessage");
        assert_eq!(wrapped["message.chat_id"], 42);
        assert_eq!(wrapped["message.sender_user_id"], 7);
        assert_eq!(wrapped["message.message_thread_id"], 3);

        let flat = UpdateFilter::flat("updateNewCallbackQuery")
            .chat_id(42)
            .to_json();
        assert_eq!(flat[FILTER_EVENT_TYPE_KEY], "updateNewCallbackQuery");
        assert_eq!(flat["chat_id"], 42);
        assert!(flat.get("message.chat_id").is_none());

        let any = UpdateFilter::any().to_json();
        assert_eq!(any[FILTER_EVENT_TYPE_KEY], ABI_UPDATE_EVENT);
        assert_eq!(any.as_object().unwrap().len(), 1);
    }

    /// 验收 3 的一半：内联键盘 + callback query 的关联。
    #[test]
    fn keyboard_and_callback_query_round_trip() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let request = simplify
            .send_message_with_keyboard(
                7,
                "选一个",
                vec![vec![Button::callback("A", "a"), Button::callback("B", "b")]],
            )
            .unwrap();
        assert!(request.is_valid());

        // 模拟一条 callback query 更新：chat_id 在顶层，所以要 flat
        let payload = json!({
            "@type": "updateNewCallbackQuery",
            "id": 555,
            "chat_id": 7,
            "sender_user_id": 1,
        });
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = {
            let seen = Arc::clone(&seen);
            rt.events().on(
                "updateNewCallbackQuery",
                Some(fuyubot_event::Filter::new().field("chat_id", 7i64)),
                move |payload| {
                    seen.lock().unwrap().push(payload.clone());
                    Ok(())
                },
            )
        };
        rt.publish_update(&payload).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "插件该收到回调查询");
        drop(listener);

        // 收到回调查询后回一条
        let answered = simplify
            .answer_callback_query(555, Some("好的"), true)
            .unwrap();
        assert!(answered.is_valid());

        let sent = rt.sent_requests();
        let answer = sent.last().unwrap();
        assert_eq!(answer.method, "answerCallbackQuery");
        assert_eq!(answer.params["callback_query_id"], 555);
        assert_eq!(answer.params["text"], "好的");
        assert_eq!(answer.params["show_alert"], true);

        // 发出去的那条消息确实带着键盘
        let message = &sent[0].params;
        assert_eq!(
            message["reply_markup"]["@type"],
            "replyMarkupInlineKeyboard"
        );
    }

    #[test]
    fn resolve_entities_fires_all_then_aggregates() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));

        // 起一个后台线程：看到请求就逐个回响应（模拟 TDLib）
        let responder = {
            let rt = Arc::clone(&rt);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                for _ in 0..8 {
                    std::thread::sleep(Duration::from_millis(10));
                    let pending: Vec<u64> = rt
                        .sent_requests()
                        .iter()
                        .filter(|request| {
                            request.method == "getChat" || request.method == "getUser"
                        })
                        .map(|request| request.id)
                        .filter(|id| !requests.lock().unwrap().contains(id))
                        .collect();
                    for id in pending {
                        requests.lock().unwrap().push(id);
                        rt.deliver_response(&json!({
                            "@type": "ok",
                            "@extra": id.to_string(),
                            "id": id,
                        }))
                        .unwrap();
                    }
                    if requests.lock().unwrap().len() >= 2 {
                        return;
                    }
                }
            })
        };

        let resolution = simplify
            .resolve_entities(vec![EntityId::Chat(7), EntityId::User(9)], 2_000)
            .unwrap();
        responder.join().unwrap();

        assert_eq!(resolution.entities.len(), 2);
        assert_eq!(resolution.entities[0].id, EntityId::Chat(7));
        assert_eq!(resolution.entities[1].id, EntityId::User(9));
        assert_eq!(resolution.entities[0].value["@type"], "ok");
        assert!(resolution.request_id.is_valid());

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|request| request.method.as_str()).collect();
        assert_eq!(methods, ["getChat", "getUser"]);
    }

    #[test]
    fn download_file_reports_and_stops_at_completion() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let progress = Arc::new(std::sync::Mutex::new(Vec::new()));

        let listener = {
            let progress = Arc::clone(&progress);
            rt.events().on("file_progress", None, move |payload| {
                progress.lock().unwrap().push(payload.clone());
                Ok(())
            })
        };

        let publisher = {
            let rt = Arc::clone(&rt);
            std::thread::spawn(move || {
                for (downloaded, done) in [(10u64, false), (20, false), (30, true)] {
                    std::thread::sleep(Duration::from_millis(10));
                    rt.publish_update(&json!({
                        "@type": "updateFile",
                        "file": {
                            "id": 99,
                            "expected_size": 30,
                            "local": {
                                "downloaded_size": downloaded,
                                "is_downloading_completed": done,
                            }
                        }
                    }))
                    .unwrap();
                }
            })
        };

        let download = simplify.download_file(99, 1, 2_000).unwrap();
        publisher.join().unwrap();
        drop(listener);

        assert!(download.completed, "应该等到完成");
        assert_eq!(download.file_id, 99);

        let progress = progress.lock().unwrap();
        assert_eq!(progress.len(), 3, "三条 updateFile 各推一个简化事件");
        assert_eq!(progress[0]["file_id"], 99);
        assert_eq!(progress[0]["downloaded_size"], 10);
        assert_eq!(progress[0]["completed"], false);
        assert_eq!(progress[2]["completed"], true);

        // 请求确实是 downloadFile
        let sent = rt.sent_requests();
        assert_eq!(sent[0].method, "downloadFile");
        assert_eq!(sent[0].params["file_id"], 99);
        assert_eq!(sent[0].params["priority"], 1);
    }

    /// 把 `Simplify` 真的拼出来的每一个请求，拿去和**真 `td_api.tl`** 烤出来的元数据对一遍。
    ///
    /// 这条离线就能跑（schema 是编译期烘进二进制的），而它挡的正是 HANDOFF 缺口 11 里那半
    /// ——「方法名/参数名是按 TDLib 文档写的，没人验过」。`check_request` 会抓出
    /// 不存在的方法名、把构造器当方法调、以及**拼错的参数名**。
    ///
    /// 它验不了的是「真 Telegram 认不认这些值」（那要真账号+网络），别把这条当成端到端验收。
    #[test]
    fn every_request_simplify_builds_passes_the_real_schema() {
        let rt = runtime();
        let simplify = simplify(&rt);

        // 尽可能多地把 Simplify 的请求拼法都走一遍（拿不到响应没关系，请求已经留档了）。
        simplify
            .send_message(7, MessageContent::text("hi"), &SendOptions::default())
            .unwrap();
        simplify
            .send_message(
                7,
                MessageContent::photo("/tmp/a.jpg"),
                &SendOptions::default(),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                MessageContent::document("/tmp/a.pdf"),
                &SendOptions::default(),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                MessageContent::location(1.5, 2.5),
                &SendOptions::default(),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                MessageContent::text("hi"),
                &SendOptions {
                    message_thread_id: Some(3),
                    reply_to_message_id: Some(9),
                    disable_notification: true,
                    protect_content: true,
                    reply_markup: Some(inline_keyboard(vec![vec![Button::callback("ok", "x")]])),
                    ..SendOptions::default()
                },
            )
            .unwrap();
        simplify
            .send_message_with_keyboard(7, "选一个", vec![vec![Button::callback("A", "a")]])
            .unwrap();
        simplify
            .answer_callback_query(555, Some("好的"), true)
            .unwrap();
        // 不等响应：超时也照样说明请求发出去了
        let _ = simplify.download_file(99, 1, 10);

        let schema = fuyubot_tdlib::TdApiSchema::compiled();
        let sent = rt.sent_requests();
        assert!(!sent.is_empty(), "一条请求都没留档，测试没意义");

        for request in &sent {
            let mut params = request.params.clone();
            // `@extra` 是宿主注入的，不属于方法参数；`check_request` 只跳过 `@type`。
            if let Some(object) = params.as_object_mut() {
                object.remove("@extra");
            }
            schema
                .check_request(&request.method, &params)
                .unwrap_or_else(|error| {
                    panic!(
                        "Simplify 拼的 {} 过不了真 schema：{error}\n{params}",
                        request.method
                    )
                });
        }
        println!(
            "[schema] 校验了 {} 条请求，方法名与参数名全部对上",
            sent.len()
        );
    }

    #[test]
    fn base64_matches_the_known_vectors() {
        // TDLib 的 callback data 是 base64；用 RFC 4648 的样例校对
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("foob"), "Zm9vYg==");
        assert_eq!(base64("fooba"), "Zm9vYmE=");
        assert_eq!(base64("foobar"), "Zm9vYmFy");
    }
}
