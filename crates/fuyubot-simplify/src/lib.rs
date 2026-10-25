//! TDLib 便捷封装：**只简化输入，不改变输出**。
//!
//! 参数形状刻意跟「写 TDLib 的人已经习惯的那一套」保持一致——一个方法一个参数结构体，
//! 字段用短名字（`text` / `media` / `topic_id` / `reply_to_message_id` / `reply_markup` /
//! `link_preview` / `timeout_ms`），**不暴露 TDLib 的嵌套 JSON**：
//!
//! ```ignore
//! simplify.send_message(chat_id, &SendMessageParams {
//!     text: Some(Text::markdown("*你好*")),
//!     reply_to_message_id: Some(42),
//!     ..SendMessageParams::default()
//! })?;
//! ```
//!
//! 请求本身一律经 [`HostApi::td_invoke`] / [`HostApi::td_invoke_simplified`] 发出，
//! 不绕过 ABI、也不碰 TDLib 的库；原始更新照旧全部对插件可见（这里只**额外**推简化事件）。
//!
//! ## 文本格式（Markdown / HTML）
//!
//! **没有自己的 Markdown 解析器**：带标记的文本经 [`Simplify::formatted`] 交给 TDLib 的
//! `parseTextEntities`，走的是**同步**的 [`HostApi::td_execute`]（`td_json_client_execute`）。
//! 同步很关键——`parseTextEntities` 不碰服务器，TDLib 允许这么调；换成「发请求 + 等响应通道」
//! 就会把投递线程卡住（插件通常正是在更新回调里发消息）。拿不到结果就退回纯文本。
//!
//! ## 两条必须知道的时间线
//!
//! - `td_invoke*` 返回的是**请求号**，真正的 JSON 从响应通道回来（事件类型
//!   [`RESPONSE_EVENT`](fuyubot_abi::RESPONSE_EVENT)，只发给发起请求的那个插件）。
//!   编辑类方法也一样：改完的 `Message`、以及 `Message is not modified` 这类错误都是响应。
//! - [`WaitHandle::payload_json`](fuyubot_abi::WaitHandle) 是宿主临时的线程局部缓冲，
//!   **只在下一次宿主调用之前有效**。要拿数据就立刻读，或者用本文的 `*_json` / `wait_response`
//!   （它们已经替你拷好了）。
//!
//! ## 类型提示
//!
//! TDLib 的请求/更新类型（编译期从 `td_api.tl` 生成）经 [`td_api`] 重导：
//! 调用方拿到 `Value` 之后可以直接 `serde_json::from_value::<td_api::Message>(…)`，
//! 有完整的字段文档与编辑器补全，不必自己拼裸 JSON 结构。

use std::ffi::{CStr, c_char, c_void};
use std::time::{Duration, Instant};

use fuyubot_abi::{
    FILTER_EVENT_TYPE_KEY, HostApi, RESPONSE_EVENT, RequestId, SubscriptionHandle, UPDATE_EVENT,
    WaitHandle,
};
use serde_json::{Map, Value, json};

/// TDLib 生成类型（`td_api`）与可校验元数据（`schema`）的家在 `fuyubot-tdlib-sys`；
/// 这里重导 `td_api`，让只依赖本 crate 的调用方也有类型提示。
pub use fuyubot_tdlib_sys::td_api;

/// 封装层的错误。
#[derive(Debug, thiserror::Error)]
pub enum SimplifyError {
    #[error(transparent)]
    Abi(#[from] fuyubot_abi::AbiError),
    #[error("参数串里含 NUL 字节")]
    BadString,
    #[error("相册只能放 2–10 项，收到 {0} 项")]
    AlbumSize(usize),
}

type Result<T> = std::result::Result<T, SimplifyError>;

// ── 文本 ──────────────────────────────────────────────────────────

/// 文本解析模式。
///
/// 直接映射到 TDLib 的 `TextParseMode`：带标记的文本（Markdown / MarkdownV2 / HTML）
/// 经 `parseTextEntities` 换成 `formattedText` 的 `entities`。**解析不在本 crate 里做**——
/// 没有第二套 Markdown 实现，也就不存在「我们认的语法」和「Telegram 认的语法」漂移这回事。
///
/// 代价是一次同步往返（`td_execute`，见 [`Simplify::formatted`]）：不碰服务器的方法
/// 才能这么调，所以它在事件回调里也安全。宿主没接 TDLib 时退回纯文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseMode {
    /// 纯文本：不花往返，`entities` 为空。
    #[default]
    Plain,
    Markdown,
    MarkdownV2,
    Html,
}

impl ParseMode {
    /// 转成 TDLib 的 `TextParseMode`；`Plain` 没有对应的构造器（不出请求）。
    fn to_td(self) -> Option<Value> {
        match self {
            Self::Plain => None,
            // 真 `td_api.tl`：`textParseModeMarkdown version:int32`，1 = Markdown，2 = MarkdownV2
            Self::Markdown => Some(json!({ "@type": "textParseModeMarkdown", "version": 1 })),
            Self::MarkdownV2 => Some(json!({ "@type": "textParseModeMarkdown", "version": 2 })),
            Self::Html => Some(json!({ "@type": "textParseModeHTML" })),
        }
    }
}

/// 一段文本 + 它的解析模式。
///
/// 纯文本走 [`Text::plain`]（或 `From<&str>`），带标记就走 [`Text::markdown`] /
/// [`Text::markdown_v2`] / [`Text::html`]。想显式挑模式用 [`Text::new`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub parse_mode: ParseMode,
}

impl Text {
    pub fn new(text: impl Into<String>, parse_mode: ParseMode) -> Self {
        Self {
            text: text.into(),
            parse_mode,
        }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::Plain)
    }

    /// Telegram 的经典 Markdown（`*bold*` / `_italic_` / `` `code` ``）。
    pub fn markdown(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::Markdown)
    }

    /// MarkdownV2（也是各家 Bot API 的默认版本）。
    ///
    /// ⚠️ **真库实测（TDLib 1.8.68，1.8.66 同结论）：粗体是 `*x*`，不是 `**x**`**。`**x**` 会被吞成
    /// `x` 而且**不给** bold entity，`__x__` 出来的是下划线。别按 CommonMark 的直觉写——
    /// 语法以 TDLib 为准（`crates/fuyubot-tdlib/tests/integration.rs` 里那条
    /// `execute_runs_synchronous_methods` 钉着这件事）。
    pub fn markdown_v2(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::MarkdownV2)
    }

    pub fn html(text: impl Into<String>) -> Self {
        Self::new(text, ParseMode::Html)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self::plain(text)
    }
}

/// 把一段文本当成**纯文本**包成 `formattedText`（不做任何解析，不花往返）。
///
/// 它也是 [`content_json`] / [`send_params`] 的默认渲染器：
/// 需要 Markdown 就传 [`Simplify::formatted`]。
pub fn plain_text(text: &Text) -> Value {
    json!({ "@type": "formattedText", "text": text.text, "entities": [] })
}

/// `parseTextEntities` 的请求 JSON（**唯一的** Markdown/HTML 解析入口）。
///
/// 真 `td_api.tl`：`parseTextEntities text:string parse_mode:TextParseMode = FormattedText`。
/// 纯文本没有对应的 `TextParseMode`，所以返回 `None`——别为它白跑一趟。
pub fn parse_text_request(text: &Text) -> Option<Value> {
    let parse_mode = text.parse_mode.to_td()?;
    Some(json!({
        "@type": "parseTextEntities",
        "text": text.text,
        "parse_mode": parse_mode,
    }))
}

/// TDLib 对 `parseTextEntities` 的响应 → `formattedText`。
///
/// 拿不到（没接 TDLib / 超时 / TDLib 报错 / 回了个别的东西）就**退回纯文本**：
/// 一条 Markdown 写错的文本不该让整条消息发不出去。
pub fn formatted_from(text: &Text, response: Option<&Value>) -> Value {
    match response {
        Some(value) if is_formatted_text(Some(value)) => value.clone(),
        _ => plain_text(text),
    }
}

/// TDLib 真的把文本解析成了 `formattedText` 吗（没接 TDLib / 报错都是 `false`）。
pub fn is_formatted_text(response: Option<&Value>) -> bool {
    let Some(value) = response else {
        return false;
    };
    value.get("@type").and_then(Value::as_str) == Some("formattedText")
}

// ── 媒体 ──────────────────────────────────────────────────────────

/// 要发的文件（对方的 `toTdInputFile`）。
///
/// 本地路径走 `inputFileLocal`，already-uploaded 的文件 id 走 `inputFileRemote`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputFile {
    Local { path: String },
    Remote { id: String },
}

impl InputFile {
    pub fn local(path: impl Into<String>) -> Self {
        Self::Local { path: path.into() }
    }

    pub fn remote(id: impl Into<String>) -> Self {
        Self::Remote { id: id.into() }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Local { path } => json!({ "@type": "inputFileLocal", "path": path }),
            Self::Remote { id } => json!({ "@type": "inputFileRemote", "id": id }),
        }
    }
}

/// 字符串一律当成**本地路径**（最常用的那种）。
impl From<&str> for InputFile {
    fn from(path: &str) -> Self {
        Self::local(path)
    }
}

impl From<String> for InputFile {
    fn from(path: String) -> Self {
        Self::local(path)
    }
}

/// 媒体内容（对应 TDLib 的各个 `inputMessage*` 构造器）。
///
/// 尺寸/时长这些填 `0` 也没关系（TDLib 会自己读真实值），要精确控制再填。
#[derive(Debug, Clone, PartialEq)]
pub enum Media {
    Photo {
        file: InputFile,
        caption: Option<Text>,
        width: i32,
        height: i32,
        has_spoiler: bool,
    },
    Video {
        file: InputFile,
        caption: Option<Text>,
        duration: i32,
        width: i32,
        height: i32,
        supports_streaming: bool,
        has_spoiler: bool,
    },
    Audio {
        file: InputFile,
        caption: Option<Text>,
        duration: i32,
        title: String,
        performer: String,
    },
    Document {
        file: InputFile,
        caption: Option<Text>,
    },
    Animation {
        file: InputFile,
        caption: Option<Text>,
        duration: i32,
        width: i32,
        height: i32,
        has_spoiler: bool,
    },
    Sticker {
        file: InputFile,
        emoji: String,
        width: i32,
        height: i32,
    },
    Location {
        latitude: f64,
        longitude: f64,
    },
}

impl Media {
    /// 一张图（尺寸交给 TDLib 自己判断）。
    pub fn photo(file: impl Into<InputFile>) -> Self {
        Self::Photo {
            file: file.into(),
            caption: None,
            width: 0,
            height: 0,
            has_spoiler: false,
        }
    }

    /// 一个文件（文档）。
    pub fn document(file: impl Into<InputFile>) -> Self {
        Self::Document {
            file: file.into(),
            caption: None,
        }
    }

    pub fn location(latitude: f64, longitude: f64) -> Self {
        Self::Location {
            latitude,
            longitude,
        }
    }

    /// 给带标题的媒体挂一段文本（贴纸/位置没有标题，原样返回）。
    pub fn caption(self, text: impl Into<Text>) -> Self {
        let caption = Some(text.into());
        match self {
            Self::Photo {
                file,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Photo {
                file,
                caption,
                width,
                height,
                has_spoiler,
            },
            Self::Video {
                file,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
                ..
            } => Self::Video {
                file,
                caption,
                duration,
                width,
                height,
                supports_streaming,
                has_spoiler,
            },
            Self::Audio {
                file,
                duration,
                title,
                performer,
                ..
            } => Self::Audio {
                file,
                caption,
                duration,
                title,
                performer,
            },
            Self::Document { file, .. } => Self::Document { file, caption },
            Self::Animation {
                file,
                duration,
                width,
                height,
                has_spoiler,
                ..
            } => Self::Animation {
                file,
                caption,
                duration,
                width,
                height,
                has_spoiler,
            },
            other => other,
        }
    }

    fn caption_ref(&self) -> Option<&Text> {
        match self {
            Self::Photo { caption, .. }
            | Self::Video { caption, .. }
            | Self::Audio { caption, .. }
            | Self::Document { caption, .. }
            | Self::Animation { caption, .. } => caption.as_ref(),
            Self::Sticker { .. } | Self::Location { .. } => None,
        }
    }
}

/// 媒体 → `InputMessageContent`（内容类型的形状**只有这一份实现**）。
///
/// 形状照真 `td_api.tl`：
/// - `inputMessagePhoto photo:inputPhoto caption:formattedText …`，而 `inputPhoto photo:InputFile`
///   ——**`photo` 套了两层**，内层那个才是文件；
/// - `inputMessageVideo` / `inputMessageAudio` / `inputMessageDocument` / `inputMessageAnimation`
///   同一套模式（外层内容 → 中层 `inputX` → 内层 `InputFile`）；
/// - `inputMessageSticker sticker:inputSticker emoji:string`（**没有 caption**）；
/// - **caption 在内容类型里**，不是 `sendMessage` 的顶层参数。
///
/// `render` 决定文本怎么变成 `formattedText`：纯文本用 [`plain_text`]，
/// 要走 TDLib 的 Markdown 解析就传 [`Simplify::formatted`]。
pub fn content_json(media: &Media, render: &dyn Fn(&Text) -> Value) -> Value {
    let caption = |media: &Media| media.caption_ref().map(|text| render(text));
    match media {
        Media::Photo {
            file,
            width,
            height,
            has_spoiler,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessagePhoto"));
            object.insert(
                "photo".to_owned(),
                json!({
                    "@type": "inputPhoto",
                    "photo": file.to_json(),
                    "width": width,
                    "height": height,
                }),
            );
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            object.insert("has_spoiler".to_owned(), json!(has_spoiler));
            Value::Object(object)
        }
        Media::Video {
            file,
            duration,
            width,
            height,
            supports_streaming,
            has_spoiler,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageVideo"));
            object.insert(
                "video".to_owned(),
                json!({
                    "@type": "inputVideo",
                    "video": file.to_json(),
                    "duration": duration,
                    "width": width,
                    "height": height,
                    "supports_streaming": supports_streaming,
                }),
            );
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            object.insert("has_spoiler".to_owned(), json!(has_spoiler));
            Value::Object(object)
        }
        Media::Audio {
            file,
            duration,
            title,
            performer,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageAudio"));
            object.insert(
                "audio".to_owned(),
                json!({
                    "@type": "inputAudio",
                    "audio": file.to_json(),
                    "duration": duration,
                    "title": title,
                    "performer": performer,
                }),
            );
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            Value::Object(object)
        }
        Media::Document { file, .. } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageDocument"));
            object.insert(
                "document".to_owned(),
                json!({ "@type": "inputDocument", "document": file.to_json() }),
            );
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            Value::Object(object)
        }
        Media::Animation {
            file,
            duration,
            width,
            height,
            has_spoiler,
            ..
        } => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageAnimation"));
            object.insert(
                "animation".to_owned(),
                json!({
                    "@type": "inputAnimation",
                    "animation": file.to_json(),
                    "duration": duration,
                    "width": width,
                    "height": height,
                }),
            );
            if let Some(caption) = caption(media) {
                object.insert("caption".to_owned(), caption);
            }
            object.insert("has_spoiler".to_owned(), json!(has_spoiler));
            Value::Object(object)
        }
        Media::Sticker {
            file,
            emoji,
            width,
            height,
        } => json!({
            "@type": "inputMessageSticker",
            "sticker": {
                "@type": "inputSticker",
                "sticker": file.to_json(),
                "width": width,
                "height": height,
            },
            "emoji": emoji,
        }),
        Media::Location {
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

// ── 内联键盘 ──────────────────────────────────────────────────────

/// 按钮样式（TDLib 1.8 的 `ButtonStyle`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
}

impl ButtonStyle {
    fn to_json(self) -> Value {
        let name = match self {
            Self::Default => "buttonStyleDefault",
            Self::Primary => "buttonStylePrimary",
            Self::Danger => "buttonStyleDanger",
            Self::Success => "buttonStyleSuccess",
        };
        json!({ "@type": name })
    }
}

/// 按钮的行为。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAction {
    /// 点下去回一条 callback query 给机器人（`data` 会被 base64 编码）。
    Callback { data: String },
    /// 打开链接（用户账号不能发，TDLib 会拒绝）。
    Url { url: String },
    /// 打开某个用户的主页。
    User { user_id: i64 },
    /// 打开 Web App。
    WebApp { url: String },
}

/// 一个内联键盘按钮。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyButton {
    pub text: String,
    pub action: ButtonAction,
    /// 文案前面那个自定义 emoji 的 id（`"0"` = 没有，TDLib 的约定）。
    pub emoji_id: Option<i64>,
    pub style: ButtonStyle,
}

impl ReplyButton {
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::Callback { data: data.into() },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::Url { url: url.into() },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    pub fn user(text: impl Into<String>, user_id: i64) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::User { user_id },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    pub fn web_app(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            action: ButtonAction::WebApp { url: url.into() },
            emoji_id: None,
            style: ButtonStyle::Default,
        }
    }

    /// 挂一个自定义 emoji（按钮文案前面）。
    pub fn emoji(mut self, custom_emoji_id: i64) -> Self {
        self.emoji_id = Some(custom_emoji_id);
        self
    }

    pub fn style(mut self, style: ButtonStyle) -> Self {
        self.style = style;
        self
    }

    fn to_json(&self) -> Value {
        let kind = match &self.action {
            // 真 `td_api.tl`：`inlineKeyboardButtonTypeCallback data:bytes`（JSON 里是 base64）
            ButtonAction::Callback { data } => {
                json!({ "@type": "inlineKeyboardButtonTypeCallback", "data": base64(data) })
            }
            ButtonAction::Url { url } => {
                json!({ "@type": "inlineKeyboardButtonTypeUrl", "url": url })
            }
            ButtonAction::User { user_id } => {
                json!({ "@type": "inlineKeyboardButtonTypeUser", "user_id": user_id })
            }
            ButtonAction::WebApp { url } => {
                json!({ "@type": "inlineKeyboardButtonTypeWebApp", "url": url })
            }
        };
        json!({
            "@type": "inlineKeyboardButton",
            "text": &self.text,
            "icon_custom_emoji_id": self.emoji_id.unwrap_or(0),
            "style": self.style.to_json(),
            "type": kind,
        })
    }
}

/// 二维按钮 → `replyMarkupInlineKeyboard`；空数组就是「不要键盘」。
fn reply_markup(rows: &[Vec<ReplyButton>]) -> Option<Value> {
    if rows.iter().all(|row| row.is_empty()) {
        return None;
    }
    Some(json!({
        "@type": "replyMarkupInlineKeyboard",
        "rows": rows
            .iter()
            .map(|row| Value::Array(row.iter().map(ReplyButton::to_json).collect()))
            .collect::<Vec<_>>(),
    }))
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

// ── 参数结构体（一个方法一个，字段用短名字） ──────────────────────

/// `sendMessage` 的参数。
///
/// `text` 与 `media` 二选一：都填就是「媒体 + 标题」，只填 `text` 就是纯文本。
#[derive(Debug, Clone)]
pub struct SendMessageParams {
    /// 文本（或媒体标题）。想发 Markdown 就 [`Text::markdown`]。
    pub text: Option<Text>,
    /// 媒体内容。填了就把 `text` 当标题用。
    pub media: Option<Media>,
    /// 回复哪条消息（TDLib 的 `reply_to`）。
    pub reply_to_message_id: Option<i64>,
    /// 哪个话题/楼层（TDLib 的 `topic_id`，不是 Bot API 的 `message_thread_id`）。
    pub topic_id: Option<i64>,
    /// 内联键盘（空数组 = 不带）。
    pub reply_markup: Vec<Vec<ReplyButton>>,
    /// 链接预览：`Some(false)` = 关掉；`Some(true)` / `None` = 交给 TDLib 默认（显示）。
    pub link_preview: Option<bool>,
    /// 等最终消息时的上限（[`Simplify::send_message_wait_success`] 用）。
    pub timeout_ms: u64,
}

impl SendMessageParams {
    pub fn text(text: impl Into<Text>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    pub fn media(media: impl Into<Media>) -> Self {
        Self {
            media: Some(media.into()),
            ..Self::default()
        }
    }
}

impl Default for SendMessageParams {
    fn default() -> Self {
        Self {
            text: None,
            media: None,
            reply_to_message_id: None,
            topic_id: None,
            reply_markup: Vec::new(),
            link_preview: None,
            timeout_ms: 30_000,
        }
    }
}

/// `sendMessageAlbum` 的参数（2–10 项，只有音频/文档/图片/视频能组成相册）。
///
/// ⚠️ **相册没有 `reply_markup`**：真签名是
/// `sendMessageAlbum chat_id topic_id reply_to options input_message_contents`
/// ——想给相册挂键盘只能 `editMessageReplyMarkup`，不是发的时候带。
#[derive(Debug, Clone)]
pub struct SendAlbumParams {
    pub medias: Vec<Media>,
    /// 整组共用一段标题。
    pub caption: Option<Text>,
    pub reply_to_message_id: Option<i64>,
    pub topic_id: Option<i64>,
}

impl Default for SendAlbumParams {
    fn default() -> Self {
        Self {
            medias: Vec::new(),
            caption: None,
            reply_to_message_id: None,
            topic_id: None,
        }
    }
}

/// `editMessageText` 的参数（`text` 为 `None` 时只改键盘）。
#[derive(Debug, Clone, Default)]
pub struct EditTextParams {
    pub text: Option<Text>,
    pub link_preview: Option<bool>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
}

/// `editMessageCaption` 的参数。
#[derive(Debug, Clone, Default)]
pub struct EditCaptionParams {
    pub text: Option<Text>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
}

/// `editMessageMedia` 的参数（`text` 是新媒体内容的标题）。
#[derive(Debug, Clone, Default)]
pub struct EditMediaParams {
    pub text: Option<Text>,
    pub media: Option<Media>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
}

// ── 请求组装 ──────────────────────────────────────────────────────

/// 组装 `sendMessage` 的参数（不含 `@type`，那是 [`HostApi::td_invoke_simplified`] 补的）。
///
/// 参数名以真 `td_api.tl` 为准（`sendMessage chat_id topic_id reply_to options
/// reply_markup input_message_content`）：
/// - 话题是 **`topic_id`**（`messageTopicThread`）；
/// - 回复是 **`reply_to`**（`inputMessageReplyToMessage`）；
/// - **没有顶层 `caption`**，它在内容类型里（见 [`content_json`]）。
///
/// 这一版修掉的是一次真缺陷：上面几条以前全按 **Bot API** 的样子拼
/// （`message_thread_id` / `reply_to_message_id` / 顶层 `caption`），TDLib 一个都不认。
/// `every_request_simplify_builds_passes_the_real_schema` 会挡住再犯。
pub fn send_params(
    chat_id: i64,
    params: &SendMessageParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert(
        "input_message_content".to_owned(),
        message_content(params, render),
    );
    put_common(&mut payload, &params.topic_id, &params.reply_to_message_id);
    if let Some(markup) = reply_markup(&params.reply_markup) {
        payload.insert("reply_markup".to_owned(), markup);
    }
    Value::Object(payload)
}

/// 文本 + 媒体 → `InputMessageContent`（两者都空时按空文本处理，TDLib 会拒绝）。
fn message_content(params: &SendMessageParams, render: &dyn Fn(&Text) -> Value) -> Value {
    match &params.media {
        Some(media) => content_json(&media_with_caption(media, params.text.as_ref()), render),
        None => {
            let mut object = Map::new();
            object.insert("@type".to_owned(), json!("inputMessageText"));
            object.insert(
                "text".to_owned(),
                render(params.text.as_ref().unwrap_or(&Text::plain(""))),
            );
            if params.link_preview == Some(false) {
                object.insert(
                    "link_preview_options".to_owned(),
                    json!({ "@type": "linkPreviewOptions", "is_disabled": true }),
                );
            }
            Value::Object(object)
        }
    }
}

/// 媒体的标题：`params.text` 优先，其次是媒体自带的 `caption`。
fn media_with_caption(media: &Media, text: Option<&Text>) -> Media {
    match text {
        Some(text) => media.clone().caption(text.clone()),
        None => media.clone(),
    }
}

/// `topic_id` / `reply_to` 两个共用的可选参数。
fn put_common(payload: &mut Map<String, Value>, topic_id: &Option<i64>, reply_to: &Option<i64>) {
    if let Some(thread) = topic_id {
        payload.insert(
            "topic_id".to_owned(),
            json!({ "@type": "messageTopicThread", "message_thread_id": thread }),
        );
    }
    if let Some(message) = reply_to {
        payload.insert(
            "reply_to".to_owned(),
            json!({ "@type": "inputMessageReplyToMessage", "message_id": message }),
        );
    }
}

/// 组装 `sendMessageAlbum` 的参数。
pub fn album_params(
    chat_id: i64,
    params: &SendAlbumParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let contents: Vec<Value> = params
        .medias
        .iter()
        .map(|media| {
            let media = match &params.caption {
                Some(caption) => media.clone().caption(caption.clone()),
                None => media.clone(),
            };
            content_json(&media, render)
        })
        .collect();

    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    put_common(&mut payload, &params.topic_id, &params.reply_to_message_id);
    payload.insert("input_message_contents".to_owned(), Value::Array(contents));
    Value::Object(payload)
}

/// 三个 `editMessage*` 共用的头：`chat_id` + `message_id` + 可选的 `reply_markup`。
fn edit_params(
    chat_id: i64,
    message_id: i64,
    reply_markup_rows: &[Vec<ReplyButton>],
) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert("message_id".to_owned(), json!(message_id));
    if let Some(markup) = reply_markup(reply_markup_rows) {
        payload.insert("reply_markup".to_owned(), markup);
    }
    payload
}

/// `editMessageText` 的参数（`@type` 由宿主补）。
pub fn edit_text_params(
    chat_id: i64,
    message_id: i64,
    params: &EditTextParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = edit_params(chat_id, message_id, &params.reply_markup);
    if let Some(text) = &params.text {
        let mut content = Map::new();
        content.insert("@type".to_owned(), json!("inputMessageText"));
        content.insert("text".to_owned(), render(text));
        if params.link_preview == Some(false) {
            content.insert(
                "link_preview_options".to_owned(),
                json!({ "@type": "linkPreviewOptions", "is_disabled": true }),
            );
        }
        payload.insert("input_message_content".to_owned(), Value::Object(content));
    }
    Value::Object(payload)
}

/// `editMessageCaption` 的参数。标题在**顶层** `caption`，不在 `input_message_content` 里。
pub fn edit_caption_params(
    chat_id: i64,
    message_id: i64,
    params: &EditCaptionParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = edit_params(chat_id, message_id, &params.reply_markup);
    if let Some(text) = &params.text {
        payload.insert("caption".to_owned(), render(text));
    }
    Value::Object(payload)
}

/// `editMessageMedia` 的参数。
pub fn edit_media_params(
    chat_id: i64,
    message_id: i64,
    params: &EditMediaParams,
    render: &dyn Fn(&Text) -> Value,
) -> Value {
    let mut payload = edit_params(chat_id, message_id, &params.reply_markup);
    if let Some(media) = &params.media {
        let media = media_with_caption(media, params.text.as_ref());
        payload.insert(
            "input_message_content".to_owned(),
            content_json(&media, render),
        );
    }
    Value::Object(payload)
}

// ── 等更新的过滤器 ────────────────────────────────────────────────

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

// ── 命令（插件最常见的那种入口）──────────────────────────────────

/// 一条 `updateNewMessage` 里的命令：`/start`、`/help@SomeBot 参数`。
///
/// 只认**文本消息**，并跳过自己发的（`is_outgoing`）——机器人不该响应自己。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub chat_id: i64,
    pub message_id: i64,
    /// 命令名：去掉 `/` 与 `@BotName` 后缀，已转小写。
    pub name: String,
    /// `/cmd` 后面那一段（去首尾空白，可能为空）。
    pub args: String,
    /// 整条消息文本（命令 + 参数）。
    pub text: String,
}

impl Command {
    /// 是这条命令吗（不分大小写，`/Start` 也算）。
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// 从 `updateNewMessage` 的负载里认出一条命令；不是命令就 `None`。
///
/// 字段路径照真 `td_api.tl`：`updateNewMessage.message` →
/// `content:messageText` → `text:formattedText` → `text:string`。
pub fn command_of(update: &Value) -> Option<Command> {
    let message = update.get("message")?;
    if message
        .get("is_outgoing")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let content = message.get("content")?;
    if content.get("@type").and_then(Value::as_str) != Some("messageText") {
        return None;
    }
    let text = content.get("text")?.get("text")?.as_str()?;
    let trimmed = text.trim_start();
    let token = trimmed.split_whitespace().next()?;
    // `/help@SomeBot args…` → `help`；`@` 之后是「发给哪个 bot」，不是命令的一部分。
    let name = token
        .strip_prefix('/')?
        .split('@')
        .next()
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    Some(Command {
        chat_id: message.get("chat_id")?.as_i64()?,
        message_id: message.get("id")?.as_i64()?,
        name: name.to_ascii_lowercase(),
        args: trimmed[token.len()..].trim().to_owned(),
        text: text.to_owned(),
    })
}

/// 一个活着的「命令订阅」：**drop 就是退订**（和 `Effect` 一个语义）。
///
/// 回调闭包由它自己持有——递给 ABI 的 `user_data` 指的就是这里那个 Box，
/// 退订之后一起释放。
pub struct CommandSubscription {
    host: HostApi,
    handle: SubscriptionHandle,
    /// 只负责**持有**回调；退订之前它的地址必须稳定。
    _handler: Box<Box<dyn Fn(&Command) + Send + Sync>>,
}

impl Drop for CommandSubscription {
    fn drop(&mut self) {
        if self.handle.is_valid() {
            self.host.off(self.handle);
        }
    }
}

/// 订阅 `updateNewMessage`，把每条命令交给 `handler`（非命令、自己发的都不打扰它）。
///
/// 插件在自己的 `start` 里调一次，把返回的守卫**存进插件状态**（drop 就退订）。
/// 宿主拒绝订阅时返回 `None`。
pub fn on_command(
    host: HostApi,
    handler: impl Fn(&Command) + Send + Sync + 'static,
) -> Option<CommandSubscription> {
    let mut owned: Box<Box<dyn Fn(&Command) + Send + Sync>> = Box::new(Box::new(handler));
    // 递给 ABI 的是**内层 Box 的地址**：它是瘦指针，外层 Box 活着它就稳定。
    let user_data = (&raw mut *owned).cast::<c_void>();
    let handle = host.on("updateNewMessage", None, command_trampoline, user_data);
    handle.is_valid().then_some(CommandSubscription {
        host,
        handle,
        _handler: owned,
    })
}

/// 命令订阅的 C 回调：把负载解成 [`Command`]，再交给闭包。
extern "C-unwind" fn command_trampoline(user_data: *mut c_void, payload: *const c_char) -> i32 {
    if user_data.is_null() || payload.is_null() {
        return -1;
    }
    // SAFETY: `user_data` 是 `on_command` 里那个 Box，订阅活多久它就活多久。
    let handler = unsafe { &*(user_data as *const Box<dyn Fn(&Command) + Send + Sync>) };
    // SAFETY: 宿主保证 `payload` 是 NUL 结尾的临时字符串。
    let text = unsafe { CStr::from_ptr(payload) }.to_string_lossy();
    let Ok(update) = serde_json::from_str::<Value>(&text) else {
        return 0;
    };
    if let Some(command) = command_of(&update) {
        handler(&command);
    }
    0
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
    /// 那条更新的原始 JSON（里面 `message` 就是最终消息）。
    pub update: Value,
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

    /// 把一段文本交给 **TDLib 的 `parseTextEntities`** 解析成 `formattedText`。
    ///
    /// 这是本 crate 唯一的 Markdown / HTML 转换入口：没有第二套解析器，
    /// 也就不存在「我们认的语法」和「Telegram 认的语法」漂移这回事。
    ///
    /// 走的是**同步**的 `td_execute`（`td_json_client_execute`）：`parseTextEntities` 不碰服务器，
    /// TDLib 允许同步跑它。这一点很关键——等响应那套会卡住投递线程，
    /// 而插件通常正是在更新回调里发消息。
    ///
    /// 拿不到结果（没接 TDLib、TDLib 报错、`Plain` 模式）就退回纯文本，不报错——
    /// 一条 Markdown 写错的消息不该整条发不出去。
    pub fn formatted(&self, text: &Text) -> Value {
        let Some(request) = parse_text_request(text) else {
            return plain_text(text); // Plain：没有对应的 TextParseMode，别白跑一趟
        };
        let response = self
            .host
            .td_execute(&request.to_string())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
        if !is_formatted_text(response.as_ref()) {
            self.host.log(
                fuyubot_abi::LogLevel::Warn,
                &format!(
                    "宿主没接 TDLib（或解析失败），Markdown 按纯文本发：{}",
                    text.text
                ),
            );
        }
        formatted_from(text, response.as_ref())
    }

    /// 发消息。返回**请求号**；TDLib 的临时消息 JSON 会从响应通道回来。
    ///
    /// 不阻塞，所以在更新回调里也能放心用（想要最终消息就用
    /// [`send_message_wait_success`](Self::send_message_wait_success)）。
    pub fn send_message(&self, chat_id: i64, params: &SendMessageParams) -> Result<RequestId> {
        let payload = send_params(chat_id, params, &|text: &Text| self.formatted(text));
        Ok(self
            .host
            .td_invoke_simplified("sendMessage", &payload.to_string())?)
    }

    /// 发消息并**等到** `updateMessageSendSucceeded`（拿到最终消息）。
    ///
    /// ⚠️ **阻塞**当前线程，别在更新回调里用：投递线程被自己卡住，等到的是超时。
    pub fn send_message_wait_success(
        &self,
        chat_id: i64,
        params: &SendMessageParams,
    ) -> Result<SendConfirmation> {
        let timeout_ms = params.timeout_ms;
        let request_id = self.send_message(chat_id, params)?;
        let filter = UpdateFilter::new("updateMessageSendSucceeded")
            .chat_id(chat_id)
            .to_json()
            .to_string();
        let handle = self.host.wait_for_update(&filter, timeout_ms);
        let update = payload_of(handle).unwrap_or(Value::Null);
        Ok(SendConfirmation {
            request_id,
            confirmed: handle.did_match() && !update.is_null(),
            update,
        })
    }

    /// 发一组相册（2–10 项，仅音频/文档/图片/视频可混）。
    pub fn send_message_album(&self, chat_id: i64, params: &SendAlbumParams) -> Result<RequestId> {
        if !(2..=10).contains(&params.medias.len()) {
            return Err(SimplifyError::AlbumSize(params.medias.len()));
        }
        let payload = album_params(chat_id, params, &|text: &Text| self.formatted(text));
        Ok(self
            .host
            .td_invoke_simplified("sendMessageAlbum", &payload.to_string())?)
    }

    /// 改一条消息的文本（`editMessageText`）。
    ///
    /// 返回**请求号**：编辑后的 `Message`、以及 `Message is not modified` 这类错误
    /// 都从响应通道回来（错误是 `@type == "error"` 的一条响应），要拿就
    /// [`wait_response`](Self::wait_response)。**不在这里重试**——限流该等多久是调用方的事。
    pub fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditTextParams,
    ) -> Result<RequestId> {
        let payload = edit_text_params(chat_id, message_id, params, &|text: &Text| {
            self.formatted(text)
        });
        Ok(self
            .host
            .td_invoke_simplified("editMessageText", &payload.to_string())?)
    }

    /// 改一条媒体消息的标题（`editMessageCaption`）。
    pub fn edit_message_caption(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditCaptionParams,
    ) -> Result<RequestId> {
        let payload = edit_caption_params(chat_id, message_id, params, &|text: &Text| {
            self.formatted(text)
        });
        Ok(self
            .host
            .td_invoke_simplified("editMessageCaption", &payload.to_string())?)
    }

    /// 换掉一条消息的内容（`editMessageMedia`）。
    ///
    /// 和发消息同一套 [`Media`]：内容形状只有一份实现。
    pub fn edit_message_media(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &EditMediaParams,
    ) -> Result<RequestId> {
        let payload = edit_media_params(chat_id, message_id, params, &|text: &Text| {
            self.formatted(text)
        });
        Ok(self
            .host
            .td_invoke_simplified("editMessageMedia", &payload.to_string())?)
    }

    /// 删消息（`deleteMessages`），`revoke = true` 表示对所有人撤回。
    pub fn delete_message(
        &self,
        chat_id: i64,
        message_ids: impl IntoIterator<Item = i64>,
        revoke: bool,
    ) -> Result<RequestId> {
        let params = json!({
            "chat_id": chat_id,
            "message_ids": message_ids.into_iter().collect::<Vec<_>>(),
            "revoke": revoke,
        });
        Ok(self
            .host
            .td_invoke_simplified("deleteMessages", &params.to_string())?)
    }

    /// 回一条 callback query（点了内联按钮之后必须回，否则客户端一直转圈）。
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

    // ── 等更新 / 等响应 ────────────────────────────────────────────

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

    /// 等某个请求的**响应**（`"response"` 通道按 `request_id` 过滤）。
    ///
    /// 编辑类方法靠它拿改完的 `Message`；TDLib 报错时拿到的同样是响应
    /// （`@type == "error"`，`message` 里是原因）。超时返回 `None`。
    ///
    /// ⚠️ 同样**阻塞**当前线程。
    pub fn wait_response(&self, request: RequestId, timeout_ms: u64) -> Option<Value> {
        let filter = json!({
            FILTER_EVENT_TYPE_KEY: RESPONSE_EVENT,
            "request_id": request.id,
        })
        .to_string();
        payload_of(self.host.wait_for_update(&filter, timeout_ms))
    }

    /// 取消一个还没结束的等待（从另一个线程发起）。
    pub fn cancel_wait(&self, handle: WaitHandle) {
        self.host.cancel_wait(handle);
    }

    // ── 批量实体解析 ──────────────────────────────────────────────

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
            entities.push(EntityResult {
                id,
                request: *request,
                value: self
                    .wait_response(*request, timeout_ms)
                    .unwrap_or(Value::Null),
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

    // ── 文件下载进度 ──────────────────────────────────────────────

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

    /// 七种内容**全是 `sendMessage`**，每种一个 `InputMessageContent` 形状。
    #[test]
    fn every_media_kind_becomes_one_send_message() {
        let rt = runtime();
        let simplify = simplify(&rt);

        simplify
            .send_message(7, &SendMessageParams::text("hi"))
            .unwrap();
        simplify
            .send_message(7, &SendMessageParams::media(Media::photo("/tmp/a.jpg")))
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Video {
                    file: InputFile::remote("video-id"),
                    caption: None,
                    duration: 3,
                    width: 640,
                    height: 480,
                    supports_streaming: true,
                    has_spoiler: false,
                }),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Audio {
                    file: "/tmp/a.mp3".into(),
                    caption: None,
                    duration: 12,
                    title: "标题".to_owned(),
                    performer: "作者".to_owned(),
                }),
            )
            .unwrap();
        simplify
            .send_message(7, &SendMessageParams::media(Media::document("/tmp/a.pdf")))
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Animation {
                    file: "/tmp/a.gif".into(),
                    caption: None,
                    duration: 1,
                    width: 10,
                    height: 10,
                    has_spoiler: false,
                }),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Sticker {
                    file: "/tmp/a.webp".into(),
                    emoji: "😀".to_owned(),
                    width: 512,
                    height: 512,
                }),
            )
            .unwrap();
        simplify
            .send_message(7, &SendMessageParams::media(Media::location(1.5, 2.5)))
            .unwrap();

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|request| request.method.as_str()).collect();
        // TDLib 没有 sendPhoto / sendDocument / sendLocation——那三个是 Bot API 的方法名。
        for (index, method) in methods.iter().enumerate() {
            assert_eq!(*method, "sendMessage", "第 {index} 条不是 sendMessage");
        }
        assert_eq!(methods.len(), 8);

        // 纯文本
        let text = &sent[0].params;
        assert_eq!(text["chat_id"], 7);
        assert_eq!(text["input_message_content"]["@type"], "inputMessageText");
        assert_eq!(text["input_message_content"]["text"]["text"], "hi");

        // 图片：inputMessagePhoto.photo(inputPhoto).photo(InputFile)
        assert_eq!(
            sent[1].params["input_message_content"]["photo"]["@type"],
            "inputPhoto"
        );
        assert_eq!(
            sent[1].params["input_message_content"]["photo"]["photo"]["@type"],
            "inputFileLocal"
        );
        assert_eq!(
            sent[1].params["input_message_content"]["photo"]["photo"]["path"],
            "/tmp/a.jpg"
        );
        // 远程文件走 inputFileRemote（id 而不是 path）
        assert_eq!(
            sent[2].params["input_message_content"]["video"]["video"]["@type"],
            "inputFileRemote"
        );
        assert_eq!(
            sent[2].params["input_message_content"]["video"]["video"]["id"],
            "video-id"
        );
        // 音频 / 文档 / 动图各自的构造器名
        assert_eq!(
            sent[3].params["input_message_content"]["audio"]["@type"],
            "inputAudio"
        );
        assert_eq!(
            sent[4].params["input_message_content"]["document"]["@type"],
            "inputDocument"
        );
        assert_eq!(
            sent[5].params["input_message_content"]["animation"]["@type"],
            "inputAnimation"
        );
        // 贴纸：emoji 在内容顶层，**没有 caption**
        let sticker = &sent[6].params["input_message_content"];
        assert_eq!(sticker["@type"], "inputMessageSticker");
        assert_eq!(sticker["emoji"], "😀");
        assert!(sticker.get("caption").is_none());
        // 位置：经纬度嵌在 location 里（不是顶层）
        let location = &sent[7].params["input_message_content"];
        assert_eq!(location["@type"], "inputMessageLocation");
        assert_eq!(location["location"]["latitude"], 1.5);
    }

    /// 短名字的参数（`topic_id` / `reply_to_message_id` / `reply_markup` / `link_preview`）
    /// 都要落到真 `sendMessage` 的字段名上。
    #[test]
    fn send_params_carry_topic_reply_markup_and_link_preview() {
        let rt = runtime();
        let simplify = simplify(&rt);

        simplify
            .send_message(
                7,
                &SendMessageParams {
                    text: Some(Text::plain("hi")),
                    media: None,
                    reply_to_message_id: Some(9),
                    topic_id: Some(3),
                    reply_markup: vec![vec![
                        ReplyButton::callback("ok", "x")
                            .emoji(4242)
                            .style(ButtonStyle::Primary),
                        ReplyButton::url("打开", "https://example.com"),
                    ]],
                    link_preview: Some(false),
                    timeout_ms: 30_000,
                },
            )
            .unwrap();

        let params = rt.sent_requests()[0].params.clone();
        // 真 `sendMessage` 是 `chat_id topic_id reply_to options reply_markup input_message_content`
        assert_eq!(params["topic_id"]["@type"], "messageTopicThread");
        assert_eq!(params["topic_id"]["message_thread_id"], 3);
        assert_eq!(params["reply_to"]["@type"], "inputMessageReplyToMessage");
        assert_eq!(params["reply_to"]["message_id"], 9);
        assert!(
            params.get("message_thread_id").is_none(),
            "Bot API 的名字不该出现：{params}"
        );
        assert_eq!(params["reply_markup"]["@type"], "replyMarkupInlineKeyboard");
        let ok = &params["reply_markup"]["rows"][0][0];
        assert_eq!(ok["type"]["@type"], "inlineKeyboardButtonTypeCallback");
        // callback data 是 base64（"x" → "eA=="）
        assert_eq!(ok["type"]["data"], "eA==");
        assert_eq!(ok["icon_custom_emoji_id"], 4242);
        assert_eq!(ok["style"]["@type"], "buttonStylePrimary");
        assert_eq!(
            params["reply_markup"]["rows"][0][1]["type"]["@type"],
            "inlineKeyboardButtonTypeUrl"
        );
        // 关掉预览 = inputMessageText.link_preview_options.is_disabled
        assert_eq!(
            params["input_message_content"]["link_preview_options"]["is_disabled"],
            true
        );
    }

    /// 媒体的标题：`params.text` 优先，其次媒体自带 caption；两者都走同一条解析。
    #[test]
    fn media_caption_prefers_params_text() {
        let media = Media::photo("/tmp/a.jpg").caption("<自带标题>");
        let params = SendMessageParams {
            text: Some(Text::plain("参数标题")),
            media: Some(media.clone()),
            ..SendMessageParams::default()
        };
        let content = content_json(
            &media_with_caption(params.media.as_ref().unwrap(), params.text.as_ref()),
            &plain_text,
        );
        assert_eq!(content["@type"], "inputMessagePhoto");
        assert_eq!(content["caption"]["text"], "参数标题");

        // 没给 params.text 时用媒体自带的
        let content = content_json(&media, &plain_text);
        assert_eq!(content["caption"]["text"], "<自带标题>");
    }

    /// Markdown / HTML 的转换**只有一条路**：交给 TDLib 的 `parseTextEntities`。
    ///
    /// 这里钉住三件事：模式到 `TextParseMode` 的映射（真 `td_api.tl` 的构造器名 + 版本号）、
    /// 请求本身过得了真 schema、以及拿不到结果时退回纯文本。
    #[test]
    fn parse_modes_map_to_the_real_tdlib_text_parse_modes() {
        let markdown = parse_text_request(&Text::markdown("*hi*")).unwrap();
        assert_eq!(markdown["@type"], "parseTextEntities");
        assert_eq!(markdown["text"], "*hi*");
        assert_eq!(markdown["parse_mode"]["@type"], "textParseModeMarkdown");
        assert_eq!(markdown["parse_mode"]["version"], 1);

        let v2 = parse_text_request(&Text::markdown_v2("**hi**")).unwrap();
        assert_eq!(v2["parse_mode"]["@type"], "textParseModeMarkdown");
        assert_eq!(v2["parse_mode"]["version"], 2);

        let html = parse_text_request(&Text::html("<b>hi</b>")).unwrap();
        assert_eq!(html["parse_mode"]["@type"], "textParseModeHTML");

        // 纯文本没有对应的 TextParseMode —— 不该白跑一趟
        assert!(parse_text_request(&Text::plain("hi")).is_none());

        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        for request in [markdown, v2, html] {
            let mut params = request.clone();
            params.as_object_mut().unwrap().remove("@type");
            schema
                .check_request("parseTextEntities", &params)
                .unwrap_or_else(|e| panic!("parseTextEntities 过不了真 schema：{e}\n{request}"));
        }
    }

    /// 解析结果落到 `formattedText`；拿不到就退回纯文本（Markdown 写错也不该发不出去）。
    #[test]
    fn formatted_text_comes_from_tdlib_or_falls_back_to_plain() {
        let text = Text::markdown("**hi**");
        let parsed = json!({
            "@type": "formattedText",
            "text": "hi",
            "entities": [{ "offset": 0, "length": 2, "type": { "@type": "textEntityTypeBold" } }],
        });

        // TDLib 解析成功：原样用它的结果（含 entities）
        let formatted = formatted_from(&text, Some(&parsed));
        assert_eq!(formatted, parsed);

        // 没接 TDLib / 超时：退回纯文本，原文一个字符都不丢
        let fallback = formatted_from(&text, None);
        assert_eq!(fallback["@type"], "formattedText");
        assert_eq!(fallback["text"], "**hi**");
        assert_eq!(fallback["entities"].as_array().unwrap().len(), 0);

        // TDLib 报错：同上
        let error = json!({ "@type": "error", "code": 400, "message": "Can't parse entities" });
        assert_eq!(formatted_from(&text, Some(&error))["text"], "**hi**");

        // 真的走一遍 Simplify：离线（没接 TDLib）时不 panic、按纯文本发出去
        let rt = runtime();
        let simplify = simplify(&rt);
        simplify
            .send_message(7, &SendMessageParams::text(Text::markdown("**hi**")))
            .unwrap();
        let sent = rt.sent_requests();
        assert_eq!(
            sent[0].params["input_message_content"]["text"]["text"],
            "**hi**"
        );
        assert!(
            rt.log_lines()
                .iter()
                .any(|line| line.contains("按纯文本发")),
            "退回纯文本要留一条日志：{:?}",
            rt.log_lines()
        );
    }

    /// 编辑 / 相册 / 删除：方法名与参数名按真 `td_api.tl` 拼。
    #[test]
    fn edit_album_and_delete_build_the_real_requests() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let markup = vec![vec![ReplyButton::callback("ok", "x")]];

        simplify
            .edit_message_text(
                7,
                9,
                &EditTextParams {
                    text: Some(Text::markdown("*hi*")),
                    link_preview: Some(false),
                    reply_markup: markup.clone(),
                },
            )
            .unwrap();
        simplify
            .edit_message_caption(
                7,
                9,
                &EditCaptionParams {
                    text: Some(Text::plain("新标题")),
                    reply_markup: Vec::new(),
                },
            )
            .unwrap();
        simplify
            .edit_message_media(
                7,
                9,
                &EditMediaParams {
                    text: None,
                    media: Some(Media::photo("/tmp/a.jpg")),
                    reply_markup: markup,
                },
            )
            .unwrap();
        simplify
            .send_message_album(
                7,
                &SendAlbumParams {
                    medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                    caption: Some(Text::plain("一组图")),
                    reply_to_message_id: Some(9),
                    topic_id: Some(3),
                },
            )
            .unwrap();
        simplify.delete_message(7, [9, 10], true).unwrap();

        let sent = rt.sent_requests();
        let methods: Vec<&str> = sent.iter().map(|request| request.method.as_str()).collect();
        assert_eq!(
            methods,
            [
                "editMessageText",
                "editMessageCaption",
                "editMessageMedia",
                "sendMessageAlbum",
                "deleteMessages"
            ]
        );

        // editMessageText: chat_id message_id reply_markup input_message_content
        let text = &sent[0].params;
        assert_eq!(text["chat_id"], 7);
        assert_eq!(text["message_id"], 9);
        assert_eq!(text["reply_markup"]["@type"], "replyMarkupInlineKeyboard");
        assert_eq!(text["input_message_content"]["@type"], "inputMessageText");
        assert_eq!(
            text["input_message_content"]["link_preview_options"]["is_disabled"],
            true
        );

        // 标题在**顶层** caption（formattedText），不在 input_message_content 里
        let caption = &sent[1].params;
        assert_eq!(caption["caption"]["@type"], "formattedText");
        assert_eq!(caption["caption"]["text"], "新标题");
        assert!(caption.get("input_message_content").is_none());
        assert!(caption.get("reply_markup").is_none(), "没传就不该编一个");

        // 媒体内容与发消息同一套形状
        assert_eq!(
            sent[2].params["input_message_content"]["@type"],
            "inputMessagePhoto"
        );

        // 相册：input_message_contents 是数组，共用一段标题
        let album = &sent[3].params;
        assert_eq!(album["topic_id"]["@type"], "messageTopicThread");
        assert_eq!(album["reply_to"]["message_id"], 9);
        let contents = album["input_message_contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["caption"]["text"], "一组图");
        assert_eq!(contents[1]["caption"]["text"], "一组图");

        // deleteMessages chat_id message_ids revoke
        let delete = &sent[4].params;
        assert_eq!(delete["message_ids"], json!([9, 10]));
        assert_eq!(delete["revoke"], true);
    }

    /// 相册的项目数有硬边界（TDLib 只认 2–10）。
    #[test]
    fn album_rejects_wrong_sizes() {
        let rt = runtime();
        let simplify = simplify(&rt);
        for count in [0usize, 1, 11] {
            let params = SendAlbumParams {
                medias: (0..count).map(|_| Media::photo("/tmp/a.jpg")).collect(),
                ..SendAlbumParams::default()
            };
            let error = simplify.send_message_album(7, &params).unwrap_err();
            assert!(matches!(error, SimplifyError::AlbumSize(n) if n == count));
        }
        assert!(
            rt.sent_requests().is_empty(),
            "不合法就不该发请求：{:?}",
            rt.sent_requests()
        );
    }

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

    /// 内联键盘 + callback query 的关联。
    #[test]
    fn keyboard_and_callback_query_round_trip() {
        let rt = runtime();
        let simplify = simplify(&rt);

        let request = simplify
            .send_message(
                7,
                &SendMessageParams {
                    text: Some(Text::plain("选一个")),
                    reply_markup: vec![vec![
                        ReplyButton::callback("A", "a"),
                        ReplyButton::callback("B", "b"),
                    ]],
                    ..SendMessageParams::default()
                },
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
        assert_eq!(
            sent[0].params["reply_markup"]["@type"],
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
    /// 这条离线就能跑（schema 是编译期烘进二进制的），而它挡的正是 gaps 缺口 11 里那半
    /// ——「方法名/参数名是按 TDLib 文档写的，没人验过」。`check_request` 会抓出
    /// 不存在的方法名、把构造器当方法调、以及**拼错的参数名**。
    ///
    /// 它验不了的是「真 Telegram 认不认这些值」（那要真账号+网络），别把这条当成端到端验收。
    #[test]
    fn every_request_simplify_builds_passes_the_real_schema() {
        let rt = runtime();
        let simplify = simplify(&rt);
        let markup = vec![vec![
            ReplyButton::callback("ok", "x").style(ButtonStyle::Success),
            ReplyButton::user("用户", 42),
            ReplyButton::web_app("应用", "https://example.com"),
        ]];

        // 尽可能多地把 Simplify 的请求拼法都走一遍（拿不到响应没关系，请求已经留档了）。
        simplify
            .send_message(7, &SendMessageParams::text("hi"))
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams {
                    text: Some(Text::markdown_v2("*hi*")),
                    media: None,
                    reply_to_message_id: Some(9),
                    topic_id: Some(3),
                    reply_markup: markup.clone(),
                    link_preview: Some(false),
                    timeout_ms: 30_000,
                },
            )
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams {
                    text: Some(Text::plain("标题")),
                    media: Some(Media::photo("/tmp/a.jpg")),
                    ..SendMessageParams::default()
                },
            )
            .unwrap();
        simplify
            .send_message(7, &SendMessageParams::media(Media::document("/tmp/a.pdf")))
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Video {
                    file: "/tmp/a.mp4".into(),
                    caption: Some(Text::plain("视频")),
                    duration: 3,
                    width: 640,
                    height: 480,
                    supports_streaming: true,
                    has_spoiler: true,
                }),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Audio {
                    file: "/tmp/a.mp3".into(),
                    caption: None,
                    duration: 12,
                    title: "标题".to_owned(),
                    performer: "作者".to_owned(),
                }),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Animation {
                    file: "/tmp/a.gif".into(),
                    caption: None,
                    duration: 1,
                    width: 10,
                    height: 10,
                    has_spoiler: false,
                }),
            )
            .unwrap();
        simplify
            .send_message(
                7,
                &SendMessageParams::media(Media::Sticker {
                    file: "/tmp/a.webp".into(),
                    emoji: "😀".to_owned(),
                    width: 512,
                    height: 512,
                }),
            )
            .unwrap();
        simplify
            .send_message(7, &SendMessageParams::media(Media::location(1.5, 2.5)))
            .unwrap();
        simplify
            .send_message_album(
                7,
                &SendAlbumParams {
                    medias: vec![Media::photo("/tmp/a.jpg"), Media::photo("/tmp/b.jpg")],
                    caption: Some(Text::plain("一组")),
                    reply_to_message_id: Some(9),
                    topic_id: Some(3),
                },
            )
            .unwrap();
        simplify
            .edit_message_text(
                7,
                9,
                &EditTextParams {
                    text: Some(Text::markdown_v2("*hi*")),
                    link_preview: Some(false),
                    reply_markup: markup.clone(),
                },
            )
            .unwrap();
        simplify
            .edit_message_caption(
                7,
                9,
                &EditCaptionParams {
                    text: Some(Text::plain("标题")),
                    reply_markup: Vec::new(),
                },
            )
            .unwrap();
        simplify
            .edit_message_media(
                7,
                9,
                &EditMediaParams {
                    text: Some(Text::plain("新标题")),
                    media: Some(Media::document("/tmp/a.pdf")),
                    reply_markup: markup,
                },
            )
            .unwrap();
        simplify.delete_message(7, [9, 10], true).unwrap();
        simplify
            .answer_callback_query(555, Some("好的"), true)
            .unwrap();
        // 不等响应：超时也照样说明请求发出去了
        let _ = simplify.download_file(99, 1, 10);

        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
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

    /// `/start`、`/help@Bot 参数` 这类命令的解包（三个内置插件全靠它）。
    #[test]
    fn commands_are_extracted_from_new_messages() {
        let update = json!({
            "@type": "updateNewMessage",
            "message": {
                "id": 42,
                "chat_id": 7,
                "is_outgoing": false,
                "content": {
                    "@type": "messageText",
                    "text": {
                        "@type": "formattedText",
                        "text": "/Help@FuyuBot 参数 一",
                        "entities": [],
                    },
                },
            },
        });
        let command = command_of(&update).expect("该认出 /help");
        assert_eq!(command.chat_id, 7);
        assert_eq!(command.message_id, 42);
        assert_eq!(command.name, "help");
        assert_eq!(command.args, "参数 一");
        assert!(command.is("HELP"), "命令名不分大小写");
        assert!(!command.is("start"));

        // 自己发的不算（机器人不该响应自己）
        let mut outgoing = update.clone();
        outgoing["message"]["is_outgoing"] = json!(true);
        assert!(command_of(&outgoing).is_none());

        // 不带 `/` 的文本不算
        let plain = json!({
            "message": {
                "id": 1,
                "chat_id": 7,
                "content": {
                    "@type": "messageText",
                    "text": { "@type": "formattedText", "text": "你好", "entities": [] },
                },
            },
        });
        assert!(command_of(&plain).is_none());

        // 非文本消息（图片）不算
        let photo = json!({
            "message": {
                "id": 1,
                "chat_id": 7,
                "content": { "@type": "messagePhoto" },
            },
        });
        assert!(command_of(&photo).is_none());
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
