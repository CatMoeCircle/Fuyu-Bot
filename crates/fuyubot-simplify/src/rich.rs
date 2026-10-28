//! 富消息（`messageRichMessage`）与一份**受控的** Markdown 子集转换。
//!
//! ## 为什么这里会有一份解析器
//!
//! TDLib 的富消息有三种源（`td_api.tl` 的 `RichMessageSource`）：
//!
//! - `richMessageSourceMarkdown` / `richMessageSourceHtml`：**仅 bot**，而且**由服务端解析**——
//!   TDLib 只是把源文本原样塞进 MTProto 请求（`inputRichMessageMarkdown` / `inputRichMessageHTML`），
//!   本地不产生任何中间态。所以没有任何「同步问 TDLib 要 blocks」的办法，
//!   连 `parseTextEntities` 那种 `td_execute` 捷径都没有。
//! - `richMessageSourceBlocks`：**账号无关**，块树由调用方给出。
//!
//! 于是「用户号也想写 Markdown」这件事只能自己转。本模块的 [`markdown`] 就是那个转换器，
//! 但它**只是子集**：认的语法见 [`markdown`] 的说明，不认的一律当普通文本，绝不静默近似。
//! `formattedText` 那条老链路（[`Simplify::formatted`]）**仍然没有自己的解析器**——两者别混。
//!
//! ## 两个容易搞反的地方
//!
//! - TDLib 的 `detect_automatic_blocks` 是「**传 true 才**自动识别链接等」；
//!   Bot API 的 `skip_entity_detection` 正好相反（true = 不识别）。这里默认 `true`。
//! - 发送 / 编辑都**没有**独立方法：TDLib 里就是 `sendMessage` / `editMessageText`
//!   带上 `inputMessageRichMessage`。本模块的 `send_rich_message` / `edit_rich_message`
//!   只是把这对组合包成人话，方法名不是 TDLib 的方法名。

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::{
    Media, ReplyButton, Result, Simplify, SimplifyError, Text, Topic, content_json, plain_text,
    put_common, reply_markup,
};

/// 富消息的源：对应 TDLib 的 `RichMessageSource` 三选一。
#[derive(Debug, Clone, PartialEq)]
pub enum RichSource {
    /// Markdown 源。**仅 bot**，且由服务端解析——本地拿不到块树，发前无法预览。
    Markdown { text: String, media: Vec<RichMedia> },
    /// HTML 源。同样**仅 bot**、服务端解析。
    Html { text: String, media: Vec<RichMedia> },
    /// 块树源。**任何账号都能发**；`blocks` 里的每个元素是一条 `InputPageBlock` 的 JSON。
    Blocks { blocks: Vec<Value> },
}

impl RichSource {
    fn to_json(&self) -> Value {
        match self {
            Self::Markdown { text, media } => json!({
                "@type": "richMessageSourceMarkdown",
                "text": text,
                "media": media_json(media),
            }),
            Self::Html { text, media } => json!({
                "@type": "richMessageSourceHtml",
                "text": text,
                "media": media_json(media),
            }),
            Self::Blocks { blocks } => json!({
                "@type": "richMessageSourceBlocks",
                "blocks": blocks,
            }),
        }
    }
}

/// 富消息里「被标记引用」的媒体（对应 TDLib 的 `inputRichMessageMedia`）。
///
/// 只有 `Markdown` / `Html` 源需要它：标记里用 `tg://photo?id=X`、`tg://video?id=X`、
/// `tg://audio?id=X` 引用，`id` 是 1–64 个 base64url 字符。
/// `Blocks` 源的媒体直接作块（`inputPageBlockPhoto` 等），不走这套绑定。
///
/// `media` 只允许图片 / 视频 / 音频 / 动图（TDLib 的注释限定）；
/// 本 crate 的 [`Media::Sticker`] / [`Media::Location`] 没有对应的富消息媒体。
#[derive(Debug, Clone, PartialEq)]
pub struct RichMedia {
    pub id: String,
    pub media: Media,
}

impl RichMedia {
    pub fn new(id: impl Into<String>, media: Media) -> Self {
        Self {
            id: id.into(),
            media,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "@type": "inputRichMessageMedia",
            "id": self.id,
            "media": content_json(&self.media, &|text: &Text| plain_text(text)),
        })
    }
}

fn media_json(media: &[RichMedia]) -> Vec<Value> {
    media.iter().map(RichMedia::to_json).collect()
}

/// 一条富消息（对应 TDLib 的 `inputRichMessage`）。
#[derive(Debug, Clone, PartialEq)]
pub struct RichMessage {
    pub source: RichSource,
    /// 从右往左显示（阿拉伯语 / 希伯来语等）。
    pub is_rtl: bool,
    /// 传 `true` 才自动识别链接、邮箱等（注意与 Bot API 的 `skip_entity_detection` 相反）。
    pub detect_automatic_blocks: bool,
}

impl Default for RichMessage {
    fn default() -> Self {
        Self {
            source: RichSource::Blocks { blocks: Vec::new() },
            is_rtl: false,
            detect_automatic_blocks: true,
        }
    }
}

impl RichMessage {
    /// 把 Markdown **在本地**转成块树。
    ///
    /// 这是唯一「账号无关」的写法：转出来的 `InputPageBlock` 谁都能发。
    /// 语法是 [`markdown`] 里写的那个子集——不是 Telegram 服务端的完整方言。
    pub fn from_markdown(markdown: impl AsRef<str>) -> Self {
        Self {
            source: RichSource::Blocks {
                blocks: markdown::to_blocks(markdown.as_ref()),
            },
            ..Self::default()
        }
    }

    /// 把 Markdown 原样交给**服务端**解析（`richMessageSourceMarkdown`）。
    ///
    /// ⚠️ TDLib 文档标注这段源 **for bots only**；且解析在服务端，发出去之前拿不到块树。
    /// 好处是「服务端认什么就是什么」，不用担心我们的子集和 Telegram 的方言漂移。
    pub fn server_markdown(markdown: impl Into<String>) -> Self {
        Self {
            source: RichSource::Markdown {
                text: markdown.into(),
                media: Vec::new(),
            },
            ..Self::default()
        }
    }

    /// 把 HTML 原样交给服务端解析（同样是 **for bots only**）。
    pub fn server_html(html: impl Into<String>) -> Self {
        Self {
            source: RichSource::Html {
                text: html.into(),
                media: Vec::new(),
            },
            ..Self::default()
        }
    }

    /// 直接给块树（每项是一条 `InputPageBlock` 的 JSON）。
    ///
    /// 类型提示可以借 `td_api::InputPageBlock`：它是 `Serialize` 的，
    /// `serde_json::to_value(block)` 就是这里要的形状。
    pub fn from_blocks(blocks: Vec<Value>) -> Self {
        Self {
            source: RichSource::Blocks { blocks },
            ..Self::default()
        }
    }

    /// 给 Markdown / HTML 源挂媒体绑定（`Blocks` 源直接忽略——它的媒体在块里）。
    pub fn with_media(mut self, media: Vec<RichMedia>) -> Self {
        match &mut self.source {
            RichSource::Markdown { media: current, .. }
            | RichSource::Html { media: current, .. } => *current = media,
            RichSource::Blocks { .. } => {}
        }
        self
    }

    pub fn rtl(mut self, is_rtl: bool) -> Self {
        self.is_rtl = is_rtl;
        self
    }

    /// 追加一块（任意 `InputPageBlock` 的 JSON）。
    ///
    /// 只对 [`RichSource::Blocks`] 成立：`Markdown` / `Html` 源的块树在**服务端**，
    /// 本地根本没有可插入的位置，所以这里返回错误而不是静默丢弃。
    pub fn with_block(mut self, block: Value) -> Result<Self> {
        match &mut self.source {
            RichSource::Blocks { blocks } => blocks.push(block),
            _ => return Err(SimplifyError::RichSourceNotBlocks),
        }
        Ok(self)
    }

    /// 追加一行按钮（`inputPageBlockButtonRow`）。
    ///
    /// `align = None` 时按钮**铺满整行**（TDLib 里就是 `align: null`）。
    /// 按钮行的位置有意义：它在块树里的顺序就是显示顺序，所以通常是最后一行。
    pub fn with_button_row(
        mut self,
        buttons: Vec<RichButton>,
        align: Option<RichButtonAlign>,
    ) -> Result<Self> {
        if buttons.is_empty() {
            return Err(SimplifyError::EmptyButtonRow);
        }
        let row = json!({
            "@type": "inputPageBlockButtonRow",
            "buttons": buttons.iter().map(RichButton::to_json).collect::<Vec<_>>(),
            "align": align.map(RichButtonAlign::to_json),
        });
        match &mut self.source {
            RichSource::Blocks { blocks } => blocks.push(row),
            _ => return Err(SimplifyError::RichSourceNotBlocks),
        }
        Ok(self)
    }

    /// 追加一行**铺满整行**的按钮，最常用的一条捷径。
    pub fn with_buttons(self, buttons: Vec<RichButton>) -> Result<Self> {
        self.with_button_row(buttons, None)
    }

    pub fn detect_automatic_blocks(mut self, detect: bool) -> Self {
        self.detect_automatic_blocks = detect;
        self
    }

    /// 空消息：TDLib 一定会拒（`Rich message must be non-empty`），提前挡掉更省一次往返。
    pub fn is_empty(&self) -> bool {
        match &self.source {
            RichSource::Markdown { text, .. } | RichSource::Html { text, .. } => {
                text.trim().is_empty()
            }
            RichSource::Blocks { blocks } => blocks.is_empty(),
        }
    }

    /// 转成 TDLib 的 `inputRichMessage`。
    pub fn to_json(&self) -> Value {
        json!({
            "@type": "inputRichMessage",
            "source": self.source.to_json(),
            "is_rtl": self.is_rtl,
            "detect_automatic_blocks": self.detect_automatic_blocks,
        })
    }
}

/// 按钮样式（TDLib `ButtonStyle` 的 5 个构造器）。
///
/// `Link` 在 TDLib 里只允许 callback 按钮用（`inlineButton` 的注释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RichButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
    Success,
    Link,
}

impl RichButtonStyle {
    fn to_json(self) -> Value {
        let name = match self {
            Self::Default => "buttonStyleDefault",
            Self::Primary => "buttonStylePrimary",
            Self::Danger => "buttonStyleDanger",
            Self::Success => "buttonStyleSuccess",
            Self::Link => "buttonStyleLink",
        };
        json!({ "@type": name })
    }
}

/// 按钮动作：`InlineKeyboardButtonType` 里**发送侧**常用的那几个。
///
/// 账号限制（`inlineButton` 的注释）：常规用户只能用 `Url` / `User` / `CopyText`；
/// `Callback` / `WebApp` 是 bot 才能发的。这里不拦——拦了就没法给 bot 写代码，
/// 错误交给服务端（和 md/html 源的 `for bots only` 一个态度）。
///
/// 没做的两个：`LoginUrl`（要 `getLoginUrlInfo` 那一整套登录流程）和
/// `SwitchInline`（要 `TargetChat`）。真要用就塞 [`RichButtonAction::Other`]。
#[derive(Debug, Clone, PartialEq)]
pub enum RichButtonAction {
    /// 打开链接（HTTP 或 `tg://`）。
    Url(String),
    /// 回调（**bot only**）：用户点击后推送 `updateNewCallbackQuery`，`data` 是原样带回的 `bytes`。
    Callback(String),
    /// 打开 Web App（**bot only**）。
    WebApp(String),
    /// 打开某个用户的主页。
    User(i64),
    /// 把 `text` 复制到剪贴板。
    CopyText(String),
    /// 其余构造器（`inlineKeyboardButtonTypeLoginUrl` / `SwitchInline` …）原样给 JSON。
    Other(Value),
}

impl RichButtonAction {
    fn to_json(&self) -> Value {
        match self {
            Self::Url(url) => json!({ "@type": "inlineKeyboardButtonTypeUrl", "url": url }),
            Self::Callback(data) => json!({
                "@type": "inlineKeyboardButtonTypeCallback",
                "data": crate::base64(data),
            }),
            Self::WebApp(url) => json!({ "@type": "inlineKeyboardButtonTypeWebApp", "url": url }),
            Self::User(user_id) => json!({
                "@type": "inlineKeyboardButtonTypeUser",
                "user_id": user_id,
            }),
            Self::CopyText(text) => json!({
                "@type": "inlineKeyboardButtonTypeCopyText",
                "text": text,
            }),
            Self::Other(value) => value.clone(),
        }
    }
}

/// 富消息里的一颗按钮（TDLib 的 `inlineButton`）。
///
/// ⚠️ 两个和普通 inline 键盘不同的地方：
/// - `text` 是 `RichText`，但 TDLib 只接受 `richTexts` / `richTextPlain` /
///   `richTextCustomEmoji`——**按钮文字不能加粗**，所以这里只给纯文本。
/// - `style` 是**必填**字段，没有默认值。
#[derive(Debug, Clone, PartialEq)]
pub struct RichButton {
    pub text: String,
    pub style: RichButtonStyle,
    pub action: RichButtonAction,
    /// 富文本按钮文字（HTML 的 `<tg-button>` 里可以有 `<tg-time>` / `<tg-emoji>`）。
    /// `Some` 时**覆盖** `text`；用 [`RichButton::with_rich_text`] 设置。
    pub rich_text: Option<Value>,
}

impl RichButton {
    pub fn new(text: impl Into<String>, action: RichButtonAction) -> Self {
        Self {
            text: text.into(),
            style: RichButtonStyle::Default,
            action,
            rich_text: None,
        }
    }

    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self::new(text, RichButtonAction::Url(url.into()))
    }

    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self::new(text, RichButtonAction::Callback(data.into()))
    }

    pub fn user(text: impl Into<String>, user_id: i64) -> Self {
        Self::new(text, RichButtonAction::User(user_id))
    }

    pub fn copy_text(text: impl Into<String>, copied: impl Into<String>) -> Self {
        Self::new(text, RichButtonAction::CopyText(copied.into()))
    }

    pub fn style(mut self, style: RichButtonStyle) -> Self {
        self.style = style;
        self
    }

    /// 用富文本当按钮文字（`inlineButton.text` 只接受 `richTexts` / `richTextPlain` /
    /// `richTextCustomEmoji`，所以能用的表达式很有限）。
    pub fn with_rich_text(mut self, text: Value) -> Self {
        self.rich_text = Some(text);
        self
    }

    pub(crate) fn to_json(&self) -> Value {
        let text = self
            .rich_text
            .clone()
            .unwrap_or_else(|| json!({ "@type": "richTextPlain", "text": self.text }));
        json!({
            "@type": "inlineButton",
            "text": text,
            "style": self.style.to_json(),
            "type": self.action.to_json(),
        })
    }
}

/// 按钮行的水平对齐。`None`（[`RichMessage::with_buttons`]）表示**铺满整行**，
/// 对应 TDLib 里 `align = null`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RichButtonAlign {
    Start,
    Center,
    End,
}

impl RichButtonAlign {
    fn to_json(self) -> Value {
        let name = match self {
            Self::Start => "pageBlockHorizontalAlignmentLeft",
            Self::Center => "pageBlockHorizontalAlignmentCenter",
            Self::End => "pageBlockHorizontalAlignmentRight",
        };
        json!({ "@type": name })
    }
}

/// `RichText` 的结构化构造器：TDLib 的 **31 个表达式**全都有入口。
///
/// Markdown 子集只覆盖其中 7 个（`plain` / `bold` / `italic` / `strikethrough` /
/// `fixed` / `url`，以及 `richTexts` 拼接），其余 24 个在 Markdown 里没有语法。
/// 想给块的 `text` 加下划线、剧透、上下标、mention、hashtag、公式、锚点、自定义 emoji，
/// 就先用这里的函数拼 `RichText`，再塞进块 JSON（[`RichMessage::with_block`]）。
///
/// 所有函数返回的都是**已经序列化好的 `RichText`**（`Value`），可任意嵌套：
/// `richtext::bold(richtext::italic(richtext::plain("x")))`。
///
/// 两个发不出去的：`richTextDiff`（TDLib 注明 *not supported in inputRichMessage*）、
/// `richTextIcon`（*instant view only*）。构造器照样给，但别指望服务端接受。
pub mod richtext {
    use serde_json::{Value, json};

    use super::RichButton;

    /// `richTextPlain`
    pub fn plain(text: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextPlain", "text": text.as_ref() })
    }

    /// `richTextBold`
    pub fn bold(text: Value) -> Value {
        json!({ "@type": "richTextBold", "text": text })
    }

    /// `richTextItalic`
    pub fn italic(text: Value) -> Value {
        json!({ "@type": "richTextItalic", "text": text })
    }

    /// `richTextUnderline`（Markdown 里没有语法）
    pub fn underline(text: Value) -> Value {
        json!({ "@type": "richTextUnderline", "text": text })
    }

    /// `richTextStrikethrough`
    pub fn strikethrough(text: Value) -> Value {
        json!({ "@type": "richTextStrikethrough", "text": text })
    }

    /// `richTextSpoiler`：点击前是模糊的（Markdown 里没有语法）
    pub fn spoiler(text: Value) -> Value {
        json!({ "@type": "richTextSpoiler", "text": text })
    }

    /// `richTextSubscript`
    pub fn subscript(text: Value) -> Value {
        json!({ "@type": "richTextSubscript", "text": text })
    }

    /// `richTextSuperscript`
    pub fn superscript(text: Value) -> Value {
        json!({ "@type": "richTextSuperscript", "text": text })
    }

    /// `richTextMarked`：高亮标记（客户端按主题着色）
    pub fn marked(text: Value) -> Value {
        json!({ "@type": "richTextMarked", "text": text })
    }

    /// `richTextFixed`：等宽（Markdown 的行内 `` `code` ``）
    pub fn fixed(text: Value) -> Value {
        json!({ "@type": "richTextFixed", "text": text })
    }

    /// `richTextDateTime`
    pub fn date_time(text: Value, unix_time: i32, format: DateTimeFormat) -> Value {
        json!({
            "@type": "richTextDateTime",
            "text": text,
            "unix_time": unix_time,
            "formatting_type": format.to_json(),
        })
    }

    /// `richTextMention`：`@username`
    pub fn mention(text: Value, username: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextMention", "text": text, "username": username.as_ref() })
    }

    /// `richTextHashtag`
    pub fn hashtag(text: Value, hashtag: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextHashtag", "text": text, "hashtag": hashtag.as_ref() })
    }

    /// `richTextCashtag`：`$USD`
    pub fn cashtag(text: Value, cashtag: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextCashtag", "text": text, "cashtag": cashtag.as_ref() })
    }

    /// `richTextBankCardNumber`
    pub fn bank_card_number(text: Value, number: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextBankCardNumber",
            "text": text,
            "bank_card_number": number.as_ref(),
        })
    }

    /// `richTextTonAddress`（1.8.68 新增）
    pub fn ton_address(text: Value, address: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextTonAddress", "text": text, "address": address.as_ref() })
    }

    /// `richTextBotCommand`：`/cmd`
    pub fn bot_command(text: Value, command: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextBotCommand",
            "text": text,
            "bot_command": command.as_ref(),
        })
    }

    /// `richTextMentionName`：链接到某个用户，但显示自定义文字
    pub fn mention_name(text: Value, user_id: i64) -> Value {
        json!({ "@type": "richTextMentionName", "text": text, "user_id": user_id })
    }

    /// `richTextUrl`。`is_cached` 是「instant view 已缓存」，发送侧一律 `false`。
    pub fn url(text: Value, url: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextUrl",
            "text": text,
            "url": url.as_ref(),
            "is_cached": false,
        })
    }

    /// `richTextEmailAddress`
    pub fn email_address(text: Value, email: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextEmailAddress",
            "text": text,
            "email_address": email.as_ref(),
        })
    }

    /// `richTextPhoneNumber`
    pub fn phone_number(text: Value, phone: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextPhoneNumber",
            "text": text,
            "phone_number": phone.as_ref(),
        })
    }

    /// `richTextCustomEmoji`：**只有这个表达式没有 `text` 子节点**，只有替代文本。
    pub fn custom_emoji(custom_emoji_id: i64, alternative_text: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextCustomEmoji",
            "custom_emoji_id": custom_emoji_id,
            "alternative_text": alternative_text.as_ref(),
        })
    }

    /// `richTextIcon`：文内小图标（*instant view only*）。`document` 是已存在的
    /// `document` 对象（不是 `inputDocument`）。
    pub fn icon(document: Value, width: i32, height: i32) -> Value {
        json!({ "@type": "richTextIcon", "document": document, "width": width, "height": height })
    }

    /// `richTextMathematicalExpression`：LaTeX。
    pub fn mathematical_expression(expression: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextMathematicalExpression", "expression": expression.as_ref() })
    }

    /// `richTextButton`：把按钮**嵌进文本**里（和 `inputPageBlockButtonRow` 不同——
    /// 那个是独占一行）。1.8.68 新增。
    pub fn button(button: &RichButton) -> Value {
        json!({ "@type": "richTextButton", "button": button.to_json() })
    }

    /// `richTextDiff`：改写痕迹。**TDLib 明确不支持在 `inputRichMessage` 里用**，
    /// 只用于接收侧的编辑对比。
    pub fn diff(text: Value, old_text: Value) -> Value {
        json!({ "@type": "richTextDiff", "text": text, "old_text": old_text })
    }

    /// `richTextReference`：命名引用（正文里那种 `[1]` 角标）。
    pub fn reference(name: impl AsRef<str>, text: Value) -> Value {
        json!({ "@type": "richTextReference", "name": name.as_ref(), "text": text })
    }

    /// `richTextReferenceLink`：跳到同页引用的链接。
    pub fn reference_link(
        text: Value,
        reference_name: impl AsRef<str>,
        url: impl AsRef<str>,
    ) -> Value {
        json!({
            "@type": "richTextReferenceLink",
            "text": text,
            "reference_name": reference_name.as_ref(),
            "url": url.as_ref(),
        })
    }

    /// `richTextAnchor`：锚点本体（配 `inputPageBlockAnchor` 用）。
    pub fn anchor(name: impl AsRef<str>) -> Value {
        json!({ "@type": "richTextAnchor", "name": name.as_ref() })
    }

    /// `richTextAnchorLink`：跳到同页锚点。`anchor_name` 为空表示回到顶部。
    pub fn anchor_link(text: Value, anchor_name: impl AsRef<str>, url: impl AsRef<str>) -> Value {
        json!({
            "@type": "richTextAnchorLink",
            "text": text,
            "anchor_name": anchor_name.as_ref(),
            "url": url.as_ref(),
        })
    }

    /// `richTexts`：把多段拼起来。单元素也合法，但 TDLib 接受裸 `RichText`，
    /// 所以只有 ≥2 段时才真正需要它。
    pub fn concat(texts: Vec<Value>) -> Value {
        json!({ "@type": "richTexts", "texts": texts })
    }

    /// `DateTimePartPrecision`：日期 / 时间显示到哪一位。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DateTimePrecision {
        None,
        Short,
        Long,
    }

    impl DateTimePrecision {
        fn to_json(self) -> Value {
            let name = match self {
                Self::None => "dateTimePartPrecisionNone",
                Self::Short => "dateTimePartPrecisionShort",
                Self::Long => "dateTimePartPrecisionLong",
            };
            json!({ "@type": name })
        }
    }

    /// `DateTimeFormattingType`：相对时间，或绝对时间戳。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DateTimeFormat {
        /// 「3 小时前」这种相对显示。
        Relative,
        /// 绝对时间戳；两个精度分别控制「时间」与「日期」显示到哪一位。
        Absolute {
            time: DateTimePrecision,
            date: DateTimePrecision,
            show_day_of_week: bool,
        },
    }

    impl DateTimeFormat {
        fn to_json(self) -> Value {
            match self {
                Self::Relative => json!({ "@type": "dateTimeFormattingTypeRelative" }),
                Self::Absolute {
                    time,
                    date,
                    show_day_of_week,
                } => json!({
                    "@type": "dateTimeFormattingTypeAbsolute",
                    "time_precision": time.to_json(),
                    "date_precision": date.to_json(),
                    "show_day_of_week": show_day_of_week,
                }),
            }
        }
    }
}

/// `sendMessage` / `editMessageText` 的 `input_message_content`：`inputMessageRichMessage`。
pub fn rich_content_json(message: &RichMessage, clear_draft: bool) -> Value {
    json!({
        "@type": "inputMessageRichMessage",
        "message": message.to_json(),
        "clear_draft": clear_draft,
    })
}

/// 发 / 改富消息的参数。
///
/// 与 [`SendMessageParams`](crate::SendMessageParams) 分开：那边是「文本 + 媒体」二选一，
/// 这里只有富消息，混在一起会让互斥规则变得没人记得住。
#[derive(Debug, Clone)]
pub struct RichMessageParams {
    pub message: RichMessage,
    pub reply_to_message_id: Option<i64>,
    /// 发到哪个话题（见 [`Topic`]）。
    pub topic_id: Option<Topic>,
    pub reply_markup: Vec<Vec<ReplyButton>>,
    /// 发的时候顺手删掉草稿（TDLib 的 `clear_draft`）。
    pub clear_draft: bool,
    /// 等最终消息时的上限（[`Simplify::send_rich_message_wait_success`] 用）。
    pub timeout_ms: u64,
}

impl RichMessageParams {
    pub fn new(message: RichMessage) -> Self {
        Self {
            message,
            reply_to_message_id: None,
            topic_id: None,
            reply_markup: Vec::new(),
            clear_draft: false,
            timeout_ms: 30_000,
        }
    }

    /// 最常用的一条捷径：Markdown 在本地转块树后发送。
    pub fn markdown(markdown: impl AsRef<str>) -> Self {
        Self::new(RichMessage::from_markdown(markdown))
    }

    /// Markdown + 末尾一行按钮（本地转块树，任何账号都能发）。
    ///
    /// 按钮在 Markdown 里没有语法，只能作为块追加——所以这里是显式的第二个参数，
    /// 而不是去猜 `[文字](url)` 到底该是链接还是按钮。空 `buttons` 等于没按钮。
    pub fn markdown_with_buttons(markdown: impl AsRef<str>, buttons: Vec<RichButton>) -> Self {
        let text = markdown.as_ref();
        let message = RichMessage::from_markdown(text);
        // `from_markdown` 一定是 Blocks 源，所以 `with_buttons` 唯一的失败是空按钮行。
        let message = match message.with_buttons(buttons) {
            Ok(message) => message,
            Err(_) => RichMessage::from_markdown(text),
        };
        Self::new(message)
    }
}

impl Default for RichMessageParams {
    fn default() -> Self {
        Self::new(RichMessage::default())
    }
}

/// 组装 `sendMessage` 的参数（富消息版）。
///
/// 参数名以真 `td_api.tl` 为准：`chat_id topic_id reply_to options reply_markup
/// input_message_content`；富消息在 `input_message_content` 里，不在顶层。
pub fn rich_send_params(chat_id: i64, params: &RichMessageParams) -> Value {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert(
        "input_message_content".to_owned(),
        rich_content_json(&params.message, params.clear_draft),
    );
    put_common(&mut payload, &params.topic_id, &params.reply_to_message_id);
    if let Some(markup) = reply_markup(&params.reply_markup) {
        payload.insert("reply_markup".to_owned(), markup);
    }
    Value::Object(payload)
}

/// 组装 `editMessageText` 的参数（富消息版）。
pub fn rich_edit_params(chat_id: i64, message_id: i64, params: &RichMessageParams) -> Value {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), json!(chat_id));
    payload.insert("message_id".to_owned(), json!(message_id));
    payload.insert(
        "input_message_content".to_owned(),
        rich_content_json(&params.message, params.clear_draft),
    );
    if let Some(markup) = reply_markup(&params.reply_markup) {
        payload.insert("reply_markup".to_owned(), markup);
    }
    Value::Object(payload)
}

impl Simplify {
    /// 发一条富消息并**等到发送结局**：成功返回最终 `Message`，失败 / 超时返回错误。
    ///
    /// TDLib 没有 `sendRichMessage`——这里就是 `sendMessage` + `inputMessageRichMessage`。
    /// 富消息的错误（超长、块太多、md 写法不被接受）**全部**只能等到结局才知道：
    /// 本地没有解析器可以预判（`from_markdown` 除外，它至少能保证块树是我们自己产的）。
    ///
    /// ⚠️ **阻塞**当前线程，回调里请用 [`send_rich_message_then`](Self::send_rich_message_then)。
    pub fn send_rich_message(&self, chat_id: i64, params: &RichMessageParams) -> Result<Value> {
        if params.message.is_empty() {
            return Err(SimplifyError::EmptyRichMessage);
        }
        let payload = rich_send_params(chat_id, params);
        let request_id = self
            .host()
            .td_invoke_simplified(crate::td_api::SendMessage::METHOD, &payload.to_string())?;
        let deadline = Instant::now() + Duration::from_millis(params.timeout_ms);
        let pending = self.await_response(request_id, deadline, params.timeout_ms)?;
        self.await_outcome(
            chat_id,
            pending.get("id").and_then(Value::as_i64),
            deadline,
            params.timeout_ms,
        )
    }

    /// [`send_rich_message`](Self::send_rich_message) 的异步版：不阻塞，结局走闭包。
    ///
    /// 返回那个线程的句柄：`stop()` 里要 join 掉它（卸载之后还跑库里的代码是 UB）。
    pub fn send_rich_message_then(
        &self,
        chat_id: i64,
        params: &RichMessageParams,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        crate::spawn_then(
            move || simplify.send_rich_message(chat_id, &params),
            on_done,
        )
    }

    /// 把一条消息改成富消息（`editMessageText` + `inputMessageRichMessage`），
    /// 等到响应返回改完的 `Message`。
    ///
    /// TDLib 没有 `editRichMessage`；`editMessageText` 的 `input_message_content`
    /// 明确接受 `inputMessageText` 或 `inputMessageRichMessage`。
    pub fn edit_rich_message(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &RichMessageParams,
    ) -> Result<Value> {
        if params.message.is_empty() {
            return Err(SimplifyError::EmptyRichMessage);
        }
        let payload = rich_edit_params(chat_id, message_id, params);
        let request_id = self
            .host()
            .td_invoke_simplified(crate::td_api::EditMessageText::METHOD, &payload.to_string())?;
        self.await_response(
            request_id,
            Instant::now() + Duration::from_millis(params.timeout_ms),
            params.timeout_ms,
        )
    }

    /// [`edit_rich_message`](Self::edit_rich_message) 的异步版：不阻塞，结果走闭包。
    pub fn edit_rich_message_then(
        &self,
        chat_id: i64,
        message_id: i64,
        params: &RichMessageParams,
        on_done: impl FnOnce(Result<Value>) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let simplify = *self;
        let params = params.clone();
        crate::spawn_then(
            move || simplify.edit_rich_message(chat_id, message_id, &params),
            on_done,
        )
    }
}

/// Markdown → `InputPageBlock` 的**受控子集**。
///
/// ## 认什么（对齐 Bot API 的 Rich Markdown）
///
/// 语法以 <https://core.telegram.org/bots/api/rich-messages> 的
/// *Rich Markdown style* 为准；文档明确「Markdown 里可以夹任意 HTML」，
/// 所以**它给出的那批富消息 HTML 标签也一并解析**（块级 + 内联）。
///
/// ### 块级
///
/// | 语法 | 转成 |
/// | --- | --- |
/// | `# ` … `###### `、`<h1>` … `<h6>` | `inputPageBlockSectionHeading`（`size` = 级别） |
/// | 普通段落、`<p>` | `inputPageBlockParagraph`（段内换行保留为 `\n`） |
/// | 三反引号围栏（可带语言）、`<pre><code class="language-x">` | `inputPageBlockPreformatted` |
/// | 三反引号 + `math`、`$$ … $$`、`<tg-math-block>` | `inputPageBlockMathematicalExpression` |
/// | `> 引用`、`<blockquote>` | `inputPageBlockBlockQuote` |
/// | `<blockquote expandable>` | `inputPageBlockExpandableBlockQuote` |
/// | `<aside>文<cite>作者</cite></aside>` | `inputPageBlockPullQuote` |
/// | `<footer>`、脚注定义 `[^id]: …` | `inputPageBlockFooter`（脚注里放 `richTextReference`） |
/// | `- ` / `* ` / `+ `、`1. `、可嵌套、`[ ]` / `[x]` 任务项 | `inputPageBlockList` + `inputPageBlockListItem` |
/// | `---` / `***` / `___`、`<hr/>` | `inputPageBlockDivider` |
/// | GFM 表格（`:--:` 对齐行） | `inputPageBlockTable`（`pageBlockTableCell`） |
/// | `<details open><summary>标题</summary>…</details>` | `inputPageBlockDetails` |
/// | `<aside>` 之外的折叠块没有 md 语法 | 同上，`is_open` 看 `open` 属性 |
/// | `<tg-collage>` / `<tg-slideshow>` 包多个 `<img>` / `<video>` / `<audio>` / `<tg-document>` | `inputPageBlockCollage` / `inputPageBlockSlideshow` |
/// | `<figure>媒体<figcaption>说明<cite>署名</cite></figcaption></figure>` | 对应媒体块 + `pageBlockCaption` |
/// | 独占一行的 `![](https://… "图注")` | `inputPageBlockPhoto` 等（类型按扩展名猜，见下） |
/// | `<tg-map lat long zoom/>` | `inputPageBlockMap`（宽高固定 600×400） |
/// | `<a name="x"></a>` | `inputPageBlockAnchor` |
/// | `<tg-button-row align="left\\|center\\|right">` + `<tg-button>` | `inputPageBlockButtonRow` |
/// | `<tg-thinking>` | `inputPageBlockThinking`（仅草稿消息可用） |
///
/// ### 行内
///
/// | 语法 | 转成 |
/// | --- | --- |
/// | `**粗**` `__粗__`、`*斜*` `_斜_`、`***又粗又斜***` | `richTextBold` / `richTextItalic` / 两者嵌套 |
/// | `~~删除~~` `==标记==` `\|\|剧透\|\|` | `richTextStrikethrough` / `richTextMarked` / `richTextSpoiler` |
/// | 行内代码、`<code>` | `richTextFixed`（内部不再解析标记） |
/// | `$公式$`、`<tg-math>` | `richTextMathematicalExpression` |
/// | `<u>` `<ins>` `<sub>` `<sup>` `<mark>` `<tg-spoiler>` | 对应的 `richTextUnderline` / `Subscript` / `Superscript` / `Marked` / `Spoiler` |
/// | `[文字](url)` | `richTextUrl` |
/// | `[文字](mailto:…)` / `[文字](tel:…)` | `richTextEmailAddress` / `richTextPhoneNumber` |
/// | `[文字](tg://user?id=N)`、`<a href="tg://user?id=N">` | `richTextMentionName` |
/// | `[文字](#锚点)`、`<a href="#锚点">` | `richTextAnchorLink`（`url` 原样保留 `#锚点`） |
/// | `[^id]` | `richTextReferenceLink`（`reference_name = id`、`url = "#id"`） |
/// | `![👍](tg://emoji?id=N)`、`<tg-emoji emoji-id="N">` | `richTextCustomEmoji`（`alternative_text` 就是文字） |
/// | `![明天](tg://time?unix=N&format=wDT)`、`<tg-time unix format>` | `richTextDateTime` |
/// | `<tg-reference name="x">文字</tg-reference>` | `richTextReference` |
/// | `<b>` `<strong>` `<i>` `<em>` `<s>` `<strike>` `<del>` | 与同名 md 标记同一套表达式 |
///
/// ### 媒体类型只能按扩展名猜
///
/// Bot API 说「类型由 MIME 与 URL 决定」，那是**服务端**的行为。本地只能看后缀：
/// `.gif` → animation、`.jpg/.jpeg/.png/.webp/.bmp` → photo、`.mp4/.mov/.webm/.mkv` → video、
/// `.ogg/.oga` → voice note、`.mp3/.m4a/.flac/.wav/.opus` → audio、其余 → document。
/// URL 走 `inputFileRemote`（blocks 源没有 `tg://…?id=` 那套媒体绑定）。
///
/// ## 不认什么（一律当普通文本，绝不猜）
///
/// - 引用式链接 `[文字][id]`、标题属性 `::: warning` 之类的扩展语法；
/// - `<table>` / `<ul>` / `<ol>` / `<li>` / `<img>` 这些 HTML **块**（md 的表格与列表已经覆盖，
///   HTML 版本不做——两套并存只会让行为更难预期）；
/// - `<tg-button>` 里 `login_url` / `switch_inline_*` 两种类型（TDLib 侧形状不同，宁可不产错 JSON）；
/// - 自动识别（URL / 邮箱 / @用户名 / hashtag / cashtag / 命令 / 电话 / 银行卡号）**不本地做**：
///   `detect_automatic_blocks = true` 时由服务端与客户端负责，重复识别只会打架。
///
/// ## 已知取舍
///
/// - 段内软换行**保留**为 `\n`（聊天里更符合直觉；CommonMark 会渲染成空格）。
/// - 表格单元格的 `align` / `valign` 一定写全——TDLib 的 `pageBlockTableCell` 要求这两个字段。
/// - 块数 / 深度 / 文本长度的上限**不在这里检查**：那些是 `getOption("rich_message_*")`，
///   写死在解析器里只会在 Telegram 调整限额后变成错误来源。
/// - `tg://time` 的 `format`（ICU 风格，如 `wDT`）只能**近似**映射到 TDLib 的
///   `DateTimeFormattingType`：`w` → 显示星期、`T`/`TT` → 短/长时间、`D`/`DD` → 短/长日期。
/// - 脚注 → `richTextReferenceLink` + `inputPageBlockFooter(richTextReference)` 这一对映射
///   是**推断**（TDLib 没有脚注类型），值得真库验证；`url` 给 `#id`。
/// - 按钮行在 Markdown 里没有语法，用 [`RichMessage::with_buttons`] /
///   [`RichMessage::with_button_row`]（HTML 的 `<tg-button-row>` 也解析）。
pub mod markdown {
    use serde_json::{Value, json};

    // 行内表达式与结构化构造器共用一份实现（`richTextPlain` 只有一个来源）。
    use super::richtext;
    use super::richtext::plain;
    use super::{RichButton, RichButtonAction, RichButtonAlign, RichButtonStyle};

    /// 把 Markdown 转成 `InputPageBlock` 列表。
    ///
    /// 永不失败：认不出的语法退化成普通文本。空输入返回空 `Vec`
    /// （但 TDLib 不接受空富消息，发送前会被 [`super::RichMessage::is_empty`] 挡下）。
    pub fn to_blocks(markdown: &str) -> Vec<Value> {
        let lines: Vec<&str> = markdown.lines().collect();
        parse_blocks(&lines)
    }

    fn parse_blocks(lines: &[&str]) -> Vec<Value> {
        let mut out = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            if let Some((fence, info)) = open_fence(lines[index]) {
                let (block, next) = fenced_code(lines, index, fence, info);
                out.push(block);
                index = next;
                continue;
            }
            if lines[index].trim() == "$$" {
                let (block, next) = math_block(lines, index);
                out.push(block);
                index = next;
                continue;
            }
            // 单行块公式 `$$E = mc^2$$`（官方示例里就是这一种写法）
            if let Some(expression) = single_line_math(lines[index]) {
                out.push(json!({
                    "@type": "inputPageBlockMathematicalExpression",
                    "expression": expression.trim(),
                }));
                index += 1;
                continue;
            }
            // 块级 HTML（`<details>` / `<tg-collage>` / `<aside>` / `<footer>` / `<figure>` …）
            if let Some((block, next)) = html_block(lines, index) {
                out.push(block);
                index = next;
                continue;
            }
            // 独占一行的媒体：`![](https://… "标题")`
            if let Some(block) = media_line(lines[index]) {
                out.push(block);
                index += 1;
                continue;
            }
            // 脚注定义：`[^id]: 定义`
            if let Some(block) = footnote_def(lines[index]) {
                out.push(block);
                index += 1;
                continue;
            }
            if let Some((text, level)) = heading(lines[index]) {
                out.push(section_heading(text, level));
                index += 1;
                continue;
            }
            if is_divider(lines[index]) {
                out.push(json!({ "@type": "inputPageBlockDivider" }));
                index += 1;
                continue;
            }
            if quote_text(lines[index]).is_some() {
                let (block, next) = quote(lines, index);
                out.push(block);
                index = next;
                continue;
            }
            if let Some(marker) = list_marker(lines[index]) {
                let (block, next) = list(lines, index, marker.indent);
                out.push(block);
                index = next;
                continue;
            }
            if is_table_start(lines, index) {
                let (block, next) = table(lines, index);
                out.push(block);
                index = next;
                continue;
            }
            let (block, next) = paragraph(lines, index);
            out.push(block);
            index = next;
        }
        out
    }

    fn starts_block(lines: &[&str], index: usize) -> bool {
        let line = lines[index];
        open_fence(line).is_some()
            || line.trim() == "$$"
            || single_line_math(line).is_some()
            || is_html_block(line)
            || media_line(line).is_some()
            || footnote_def(line).is_some()
            || heading(line).is_some()
            || is_divider(line)
            || quote_text(line).is_some()
            || list_marker(line).is_some()
            || is_table_start(lines, index)
    }

    /// `$$表达式$$` 独占一行（与「`$$` 单独成行 + 多行体 + `$$`」两种写法都支持）。
    fn single_line_math(line: &str) -> Option<&str> {
        let trimmed = line.trim();
        let inner = trimmed.strip_prefix("$$")?;
        let body = inner.strip_suffix("$$")?;
        if body.trim().is_empty() {
            return None;
        }
        Some(body)
    }

    // ── 块级 HTML 与媒体 ──────────────────────────────────────────

    /// 会被当成**块**的 HTML 标签。行内标签（`<b>` / `<u>` …）不在此列，
    /// 否则段落里的一个 `<b>` 就会把整段拆成块。
    const BLOCK_TAGS: [&str; 18] = [
        "details",
        "tg-collage",
        "tg-slideshow",
        "aside",
        "footer",
        "blockquote",
        "pre",
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "tg-math-block",
        "tg-thinking",
        "figure",
        "tg-button-row",
    ];

    fn is_html_block(line: &str) -> bool {
        let Some(tag) = parse_tag(line.trim_start()) else {
            return false;
        };
        let name = tag.name.to_ascii_lowercase();
        if BLOCK_TAGS.contains(&name.as_str()) {
            return true;
        }
        if tag.self_closing {
            return matches!(name.as_str(), "hr" | "tg-map");
        }
        // `<a name="x"></a>` 是锚点块；`<a href>` 是行内链接
        name == "a" && tag.attr("name").is_some()
    }

    fn html_block(lines: &[&str], index: usize) -> Option<(Value, usize)> {
        let tag = parse_tag(lines[index].trim_start())?;
        let name = tag.name.to_ascii_lowercase();

        if tag.self_closing {
            let block = match name.as_str() {
                "hr" => json!({ "@type": "inputPageBlockDivider" }),
                "tg-map" => map_block(&tag),
                _ => return None,
            };
            return Some((block, index + 1));
        }
        if !BLOCK_TAGS.contains(&name.as_str()) && name != "a" {
            return None;
        }

        // `<a name="x"></a>` 自成一类：inner 为空，属性才是内容
        if name == "a" {
            let anchor = tag.attr("name")?;
            return Some((
                json!({ "@type": "inputPageBlockAnchor", "name": anchor }),
                index + 1,
            ));
        }

        let (inner, next) = collect_until_close(lines, index, tag.name)?;
        let block = match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<i32>().unwrap_or(1);
                section_heading(inner.trim(), level)
            }
            "p" => paragraph_block(inner.trim()),
            "footer" => json!({
                "@type": "inputPageBlockFooter",
                "footer": inline(&break_lines(inner.trim())),
            }),
            "pre" => pre_html(&inner),
            "aside" => pull_quote(&inner),
            "blockquote" => block_quote_html(&inner, &tag),
            "details" => details_block(&inner, &tag)?,
            "tg-collage" => collage_block(&inner, "inputPageBlockCollage")?,
            "tg-slideshow" => collage_block(&inner, "inputPageBlockSlideshow")?,
            "figure" => figure_block(&inner)?,
            "tg-button-row" => button_row_block(&inner, &tag)?,
            "tg-math-block" => json!({
                "@type": "inputPageBlockMathematicalExpression",
                "expression": inner.trim(),
            }),
            "tg-thinking" => json!({
                "@type": "inputPageBlockThinking",
                "text": inline(&break_lines(inner.trim())),
            }),
            _ => return None,
        };
        Some((block, next))
    }

    /// 从 `start` 行开始收集到 `</name>`，返回（标签内文本, 下一行）。
    ///
    /// 富消息的块级标签在实践里都独占行；同一行开闭也支持。
    fn collect_until_close(lines: &[&str], start: usize, name: &str) -> Option<(String, usize)> {
        let close = format!("</{name}>");
        let tag = parse_tag(lines[start].trim_start())?;
        let mut buffer = tag.rest.to_owned();
        let mut index = start;
        loop {
            if let Some(position) = buffer.find(&close) {
                return Some((buffer[..position].to_owned(), index + 1));
            }
            index += 1;
            if index >= lines.len() {
                return None;
            }
            buffer.push('\n');
            buffer.push_str(lines[index]);
        }
    }

    /// `<br>` / `<br/>` → 换行（块内文本用）。
    fn break_lines(text: &str) -> String {
        text.replace("<br />", "\n")
            .replace("<br/>", "\n")
            .replace("<br>", "\n")
    }

    fn map_block(tag: &Tag<'_>) -> Value {
        let latitude = tag.attr("lat").and_then(|value| value.parse().ok());
        let longitude = tag.attr("long").and_then(|value| value.parse().ok());
        let zoom = tag.attr("zoom").and_then(|value| value.parse().ok());
        json!({
            "@type": "inputPageBlockMap",
            "location": {
                "@type": "location",
                "latitude": latitude.unwrap_or(0.0),
                "longitude": longitude.unwrap_or(0.0),
                "horizontal_accuracy": 0.0,
            },
            "zoom": zoom.unwrap_or(14),
            "width": 600,
            "height": 400,
            "caption": Value::Null,
        })
    }

    /// `<pre>` / `<pre><code class="language-x">`。
    fn pre_html(inner: &str) -> Value {
        let trimmed = inner.trim();
        if let Some(tag) = parse_tag(trimmed) {
            if tag.name.eq_ignore_ascii_case("code") {
                let language = tag
                    .attr("class")
                    .and_then(|class| class.strip_prefix("language-"))
                    .unwrap_or("");
                if let Some((body, _)) = close_of(tag.rest, tag.name) {
                    return preformatted(body, language);
                }
            }
        }
        preformatted(trimmed, "")
    }

    fn split_cite(inner: &str) -> (&str, Option<&str>) {
        let Some(start) = inner.find("<cite>") else {
            return (inner, None);
        };
        let rest = &inner[start + "<cite>".len()..];
        let end = rest.find("</cite>").unwrap_or(rest.len());
        (&inner[..start], Some(&rest[..end]))
    }

    fn pull_quote(inner: &str) -> Value {
        let (text, credit) = split_cite(inner);
        json!({
            "@type": "inputPageBlockPullQuote",
            "text": inline(&break_lines(text.trim())),
            "credit": credit.map(|credit| inline(credit.trim())),
        })
    }

    fn block_quote_html(inner: &str, tag: &Tag<'_>) -> Value {
        let (text, credit) = split_cite(inner);
        let text = break_lines(text.trim());
        let credit = credit.map(|credit| inline(credit.trim()));
        if tag.has("expandable") {
            json!({
                "@type": "inputPageBlockExpandableBlockQuote",
                "text": inline(&text),
                "credit": credit,
            })
        } else {
            json!({
                "@type": "inputPageBlockBlockQuote",
                "blocks": vec![paragraph_block(&text)],
                "credit": credit,
            })
        }
    }

    fn details_block(inner: &str, tag: &Tag<'_>) -> Option<Value> {
        let start = inner.find("<summary>")? + "<summary>".len();
        let rest = &inner[start..];
        let end = rest.find("</summary>")?;
        let header = &rest[..end];
        let body = &rest[end + "</summary>".len()..];
        Some(json!({
            "@type": "inputPageBlockDetails",
            "header": inline(&break_lines(header.trim())),
            "blocks": parse_blocks(&body.lines().collect::<Vec<_>>()),
            "is_open": tag.has("open"),
        }))
    }

    /// 拆出 `<figcaption>…</figcaption>`（第二项是**完整**内容，可能含 `<cite>`）。
    fn split_figcaption(inner: &str) -> (&str, Option<&str>) {
        let Some(start) = inner.find("<figcaption>") else {
            return (inner, None);
        };
        let rest = &inner[start + "<figcaption>".len()..];
        let Some(end) = rest.find("</figcaption>") else {
            return (inner, None);
        };
        (&inner[..start], Some(&rest[..end]))
    }

    /// `<figcaption>文字<cite>作者</cite></figcaption>` → `pageBlockCaption`。
    fn caption_value(raw: &str) -> Option<Value> {
        let (text, credit) = split_cite(raw);
        let text = text.trim();
        if text.is_empty() && credit.is_none() {
            return None;
        }
        Some(json!({
            "@type": "pageBlockCaption",
            "text": inline(text),
            "credit": credit.map(|credit| inline(credit.trim())),
        }))
    }

    fn collage_block(inner: &str, kind: &str) -> Option<Value> {
        let (body, caption) = split_figcaption(inner);
        let mut blocks = media_tags(body);
        // 官方示例里 collage 内写的是 `![](url)`（md 媒体块形式），两种都要认
        blocks.extend(body.lines().filter_map(media_line));
        if blocks.is_empty() {
            return None;
        }
        Some(json!({
            "@type": kind,
            "blocks": blocks,
            "caption": caption.and_then(caption_value),
        }))
    }

    fn figure_block(inner: &str) -> Option<Value> {
        let (body, caption) = split_figcaption(inner);
        let mut media = media_tags(body);
        media.extend(body.lines().filter_map(media_line));
        if media.len() != 1 {
            return None;
        }
        let mut block = media.pop()?;
        if let Some(caption) = caption.and_then(caption_value) {
            block["caption"] = caption;
        }
        Some(block)
    }

    fn button_row_block(inner: &str, tag: &Tag<'_>) -> Option<Value> {
        let align = match tag.attr("align").unwrap_or("") {
            "left" => Some(RichButtonAlign::Start),
            "center" => Some(RichButtonAlign::Center),
            "right" => Some(RichButtonAlign::End),
            _ => None,
        };
        let buttons = html_buttons(inner);
        if buttons.is_empty() {
            return None;
        }
        Some(json!({
            "@type": "inputPageBlockButtonRow",
            "buttons": buttons,
            "align": align.map(RichButtonAlign::to_json),
        }))
    }

    fn html_buttons(inner: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let mut rest = inner;
        while let Some(offset) = rest.find("<tg-button") {
            rest = &rest[offset..];
            let Some(tag) = parse_tag(rest) else {
                rest = &rest[1..];
                continue;
            };
            let Some((text, after)) = close_of(tag.rest, tag.name) else {
                rest = tag.rest;
                continue;
            };
            if let Some(button) = html_button(&tag, &text) {
                out.push(button);
            }
            rest = after;
        }
        out
    }

    fn html_button(tag: &Tag<'_>, text: &str) -> Option<Value> {
        // 只做这五种：其余（login_url / switch_inline_*）在 TDLib 侧需要的字段
        // 这里没有等价语义（TDLib 的 SwitchInline 是另一个形状），宁可不产错 JSON。
        let action = match tag.attr("type").unwrap_or("") {
            "url" => {
                let url = tag.attr("url")?;
                match query_param(url, "tg://user", "id").and_then(|id| id.parse::<i64>().ok()) {
                    Some(user_id) => RichButtonAction::User(user_id),
                    None => RichButtonAction::Url(url.to_owned()),
                }
            }
            "callback_data" => RichButtonAction::Callback(tag.attr("data")?.to_owned()),
            "web_app" => RichButtonAction::WebApp(tag.attr("url")?.to_owned()),
            "copy_text" => RichButtonAction::CopyText(tag.attr("text")?.to_owned()),
            "disabled" => {
                RichButtonAction::Other(json!({ "@type": "inlineKeyboardButtonTypeDisabled" }))
            }
            _ => return None,
        };
        let style = match tag.attr("style").unwrap_or("") {
            "primary" => RichButtonStyle::Primary,
            "danger" => RichButtonStyle::Danger,
            "success" => RichButtonStyle::Success,
            "link" => RichButtonStyle::Link,
            _ => RichButtonStyle::Default,
        };
        let label = plain_text_of(text);
        Some(
            RichButton::new(label, action)
                .style(style)
                .with_rich_text(inline(&break_lines(text.trim())))
                .to_json(),
        )
    }

    /// 按钮文字取纯文本（`<tg-emoji>` / `<tg-time>` 里的可见文字；标签本身剥掉）。
    fn plain_text_of(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(ch) = rest.chars().next() {
            if ch == '<' {
                if let Some(tag) = parse_tag(rest) {
                    rest = tag.rest;
                    continue;
                }
                if let Some(end) = rest.find('>') {
                    rest = &rest[end + 1..];
                    continue;
                }
            }
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
        out
    }

    // ── 媒体块 ────────────────────────────────────────────────────

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MediaKind {
        Photo,
        Video,
        Animation,
        Audio,
        VoiceNote,
        Document,
    }

    /// 只能按扩展名猜：Bot API 说「类型由 MIME 与 URL 决定」，那是服务端的行为。
    fn media_kind_of(url: &str) -> MediaKind {
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .to_ascii_lowercase();
        let ends = |suffixes: &[&str]| suffixes.iter().any(|suffix| path.ends_with(suffix));
        if ends(&[".gif"]) {
            MediaKind::Animation
        } else if ends(&[".jpg", ".jpeg", ".png", ".webp", ".bmp"]) {
            MediaKind::Photo
        } else if ends(&[".mp4", ".mov", ".webm", ".mkv"]) {
            MediaKind::Video
        } else if ends(&[".ogg", ".oga"]) {
            MediaKind::VoiceNote
        } else if ends(&[".mp3", ".m4a", ".flac", ".wav", ".opus"]) {
            MediaKind::Audio
        } else {
            MediaKind::Document
        }
    }

    fn remote_file(url: &str) -> Value {
        json!({ "@type": "inputFileRemote", "id": url })
    }

    fn caption_of(text: &str) -> Value {
        json!({
            "@type": "pageBlockCaption",
            "text": inline(text),
            "credit": Value::Null,
        })
    }

    /// 媒体块：URL 走 `inputFileRemote`（blocks 源没有 `tg://` id 绑定）。
    fn media_block(kind: MediaKind, url: &str, caption: Option<&str>, spoiler: bool) -> Value {
        let file = remote_file(url);
        let caption = caption
            .filter(|text| !text.is_empty())
            .map(caption_of)
            .unwrap_or(Value::Null);
        match kind {
            MediaKind::Photo => json!({
                "@type": "inputPageBlockPhoto",
                "photo": {
                    "@type": "inputPhoto",
                    "photo": file,
                    "thumbnail": Value::Null,
                    "video": Value::Null,
                    "added_sticker_file_ids": [],
                    "width": 0,
                    "height": 0,
                },
                "caption": caption,
                "has_spoiler": spoiler,
            }),
            MediaKind::Video => json!({
                "@type": "inputPageBlockVideo",
                "video": {
                    "@type": "inputVideo",
                    "video": file,
                    "thumbnail": Value::Null,
                    "cover": Value::Null,
                    "start_timestamp": 0,
                    "added_sticker_file_ids": [],
                    "duration": 0,
                    "width": 0,
                    "height": 0,
                    "supports_streaming": true,
                },
                "caption": caption,
                "has_spoiler": spoiler,
            }),
            MediaKind::Animation => json!({
                "@type": "inputPageBlockAnimation",
                "animation": {
                    "@type": "inputAnimation",
                    "animation": file,
                    "thumbnail": Value::Null,
                    "added_sticker_file_ids": [],
                    "duration": 0,
                    "width": 0,
                    "height": 0,
                },
                "caption": caption,
                "has_spoiler": spoiler,
            }),
            MediaKind::Audio => json!({
                "@type": "inputPageBlockAudio",
                "audio": {
                    "@type": "inputAudio",
                    "audio": file,
                    "album_cover_thumbnail": Value::Null,
                    "duration": 0,
                    "title": "",
                    "performer": "",
                },
                "caption": caption,
            }),
            MediaKind::VoiceNote => json!({
                "@type": "inputPageBlockVoiceNote",
                "voice_note": {
                    "@type": "inputVoiceNote",
                    "voice_note": file,
                    "duration": 0,
                    "waveform": "",
                },
                "caption": caption,
            }),
            MediaKind::Document => json!({
                "@type": "inputPageBlockDocument",
                "document": {
                    "@type": "inputDocument",
                    "document": file,
                    "thumbnail": Value::Null,
                    "disable_content_type_detection": false,
                },
                "caption": caption,
            }),
        }
    }

    /// 独占一行的 `![](https://… "标题")` → 一个媒体块。
    fn media_line(line: &str) -> Option<Value> {
        let trimmed = line.trim();
        let inner = trimmed.strip_prefix("![")?;
        let close = inner.find("](")?;
        let alt = &inner[..close];
        let body = inner[close + 2..].strip_suffix(')')?;
        let (url, title) = split_url_title(body);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return None;
        }
        let caption = if title.is_empty() { alt } else { title };
        Some(media_block(media_kind_of(url), url, Some(caption), false))
    }

    fn split_url_title(body: &str) -> (&str, &str) {
        let body = body.trim();
        match body.find(" \"") {
            Some(position) => (
                body[..position].trim(),
                body[position + 2..].trim().trim_end_matches('"'),
            ),
            None => (body, ""),
        }
    }

    /// `<tg-collage>` / `<tg-slideshow>` / `<figure>` 里的媒体标签。
    fn media_tags(body: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(offset) = rest.find('<') {
            rest = &rest[offset..];
            let Some(tag) = parse_tag(rest) else {
                rest = &rest[1..];
                continue;
            };
            let name = tag.name.to_ascii_lowercase();
            let url = tag.attr("src").unwrap_or("");
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                rest = tag.rest;
                continue;
            }
            let kind = match name.as_str() {
                "img" => Some(MediaKind::Photo),
                "video" => Some(if media_kind_of(url) == MediaKind::Animation {
                    MediaKind::Animation
                } else {
                    MediaKind::Video
                }),
                "audio" => Some(if matches!(media_kind_of(url), MediaKind::VoiceNote) {
                    MediaKind::VoiceNote
                } else {
                    MediaKind::Audio
                }),
                "tg-document" => Some(MediaKind::Document),
                _ => None,
            };
            match kind {
                Some(kind) => out.push(media_block(kind, url, None, tag.has("tg-spoiler"))),
                None => {}
            }
            rest = tag.rest;
        }
        out
    }

    /// 脚注定义 `[^id]: 定义` → footer 块里放一个 `richTextReference`。
    fn footnote_def(line: &str) -> Option<Value> {
        let trimmed = line.trim_start();
        let inner = trimmed.strip_prefix("[^")?;
        let close = inner.find("]:")?;
        let name = &inner[..close];
        if name.is_empty() {
            return None;
        }
        let text = inner[close + 2..].trim();
        Some(json!({
            "@type": "inputPageBlockFooter",
            "footer": richtext::reference(name, inline(text)),
        }))
    }

    fn open_fence(line: &str) -> Option<(char, &str)> {
        let trimmed = line.trim_start();
        let first = trimmed.chars().next()?;
        if first != '`' && first != '~' {
            return None;
        }
        let count = trimmed.chars().take_while(|c| *c == first).count();
        if count < 3 {
            return None;
        }
        Some((first, trimmed[count..].trim()))
    }

    fn fenced_code(lines: &[&str], start: usize, fence: char, info: &str) -> (Value, usize) {
        let language = info.split_whitespace().next().unwrap_or("");
        let mut body: Vec<&str> = Vec::new();
        let mut index = start + 1;
        while index < lines.len() {
            let trimmed = lines[index].trim_start();
            let closed = trimmed.chars().take_while(|c| *c == fence).count() >= 3;
            if closed {
                index += 1;
                break;
            }
            body.push(lines[index]);
            index += 1;
        }
        // ```math 是**块级公式**，不是「语言叫 math 的代码块」
        if language.eq_ignore_ascii_case("math") {
            return (
                json!({
                    "@type": "inputPageBlockMathematicalExpression",
                    "expression": body.join("\n"),
                }),
                index,
            );
        }
        (preformatted(&body.join("\n"), language), index)
    }

    fn math_block(lines: &[&str], start: usize) -> (Value, usize) {
        let mut body: Vec<&str> = Vec::new();
        let mut index = start + 1;
        while index < lines.len() {
            if lines[index].trim() == "$$" {
                index += 1;
                break;
            }
            body.push(lines[index].trim());
            index += 1;
        }
        (
            json!({
                "@type": "inputPageBlockMathematicalExpression",
                "expression": body.join("\n"),
            }),
            index,
        )
    }

    fn heading(line: &str) -> Option<(&str, i32)> {
        let trimmed = line.trim_start();
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level == 0 || level > 6 {
            return None;
        }
        let rest = &trimmed[level..];
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return None;
        }
        let text = rest.trim().trim_end_matches('#').trim();
        Some((text, level as i32))
    }

    fn is_divider(line: &str) -> bool {
        let trimmed = line.trim();
        let mut chars = trimmed.chars().filter(|c| *c != ' ');
        let Some(first) = chars.next() else {
            return false;
        };
        if !matches!(first, '-' | '*' | '_') {
            return false;
        }
        let mut count = 1;
        for ch in chars {
            if ch != first {
                return false;
            }
            count += 1;
        }
        count >= 3
    }

    fn quote_text(line: &str) -> Option<&str> {
        let trimmed = line.trim_start();
        let rest = trimmed.strip_prefix('>')?;
        Some(rest.strip_prefix(' ').unwrap_or(rest))
    }

    fn quote(lines: &[&str], start: usize) -> (Value, usize) {
        let mut inner: Vec<&str> = Vec::new();
        let mut index = start;
        while index < lines.len() {
            match quote_text(lines[index]) {
                Some(text) => inner.push(text),
                None => {
                    // 引用块里的空行 = 段落分隔（只有下一行仍是引用才算）
                    let blank = lines[index].trim().is_empty();
                    let more = lines
                        .get(index + 1)
                        .is_some_and(|next| quote_text(next).is_some());
                    if blank && more {
                        inner.push("");
                    } else {
                        break;
                    }
                }
            }
            index += 1;
        }
        (
            json!({
                "@type": "inputPageBlockBlockQuote",
                "blocks": parse_blocks(&inner),
                "credit": Value::Null,
            }),
            index,
        )
    }

    fn paragraph(lines: &[&str], start: usize) -> (Value, usize) {
        let mut index = start;
        let mut text = String::new();
        while index < lines.len() {
            if lines[index].trim().is_empty() {
                break;
            }
            if index != start && starts_block(lines, index) {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(lines[index].trim());
            index += 1;
        }
        (paragraph_block(&text), index)
    }

    // ── 列表 ──────────────────────────────────────────────────────

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ListKind {
        Bullet,
        Ordered,
    }

    #[derive(Debug, Clone, Copy)]
    struct Marker<'a> {
        indent: usize,
        kind: ListKind,
        checkbox: Option<bool>,
        text: &'a str,
    }

    fn list_marker(line: &str) -> Option<Marker<'_>> {
        let indent = line.chars().take_while(|c| *c == ' ').count();
        let rest = &line[indent..];
        for prefix in ["- ", "* ", "+ "] {
            if let Some(text) = rest.strip_prefix(prefix) {
                let (checkbox, text) = checkbox_of(text);
                return Some(Marker {
                    indent,
                    kind: ListKind::Bullet,
                    checkbox,
                    text,
                });
            }
        }
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            let after = &rest[digits..];
            if let Some(text) = after
                .strip_prefix(". ")
                .or_else(|| after.strip_prefix(") "))
            {
                let (checkbox, text) = checkbox_of(text);
                return Some(Marker {
                    indent,
                    kind: ListKind::Ordered,
                    checkbox,
                    text,
                });
            }
        }
        None
    }

    fn checkbox_of(text: &str) -> (Option<bool>, &str) {
        for (prefix, checked) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
            if let Some(rest) = text.strip_prefix(prefix) {
                return (Some(checked), rest);
            }
        }
        (None, text)
    }

    /// 同一层级的连续列表项 → 一个 `inputPageBlockList`；缩进更深的部分递归成块。
    fn list(lines: &[&str], start: usize, indent: usize) -> (Value, usize) {
        let kind = list_marker(lines[start])
            .map(|marker| marker.kind)
            .unwrap_or(ListKind::Bullet);
        let mut items: Vec<Value> = Vec::new();
        let mut index = start;
        let mut ordinal = 0;

        while index < lines.len() {
            if lines[index].trim().is_empty() {
                let mut probe = index + 1;
                while probe < lines.len() && lines[probe].trim().is_empty() {
                    probe += 1;
                }
                let more = lines
                    .get(probe)
                    .and_then(|line| list_marker(line))
                    .is_some_and(|marker| marker.indent == indent && marker.kind == kind);
                if more {
                    index = probe;
                    continue;
                }
                break;
            }
            let marker = match list_marker(lines[index]) {
                Some(marker) if marker.indent == indent && marker.kind == kind => marker,
                _ => break,
            };

            // 该列表项的续行：缩进更深，或夹在其中的空行
            let mut body: Vec<&str> = Vec::new();
            let mut probe = index + 1;
            let mut blanks = 0;
            while probe < lines.len() {
                let candidate = lines[probe];
                if candidate.trim().is_empty() {
                    blanks += 1;
                    probe += 1;
                    continue;
                }
                let deeper = candidate.chars().take_while(|c| *c == ' ').count() > indent;
                let sibling = list_marker(candidate)
                    .is_some_and(|marker| marker.indent == indent && marker.kind == kind);
                if !deeper || sibling {
                    break;
                }
                for _ in 0..blanks {
                    body.push("");
                }
                blanks = 0;
                body.push(strip_indent(candidate, indent + 2));
                probe += 1;
            }

            let mut blocks: Vec<Value> = Vec::new();
            if !marker.text.trim().is_empty() || body.is_empty() {
                blocks.push(paragraph_block(marker.text));
            }
            if !body.is_empty() {
                blocks.extend(parse_blocks(&body));
            }

            ordinal += 1;
            items.push(list_item(
                blocks,
                marker.checkbox,
                if kind == ListKind::Ordered {
                    ordinal
                } else {
                    0
                },
            ));
            index = probe;
        }

        (
            json!({ "@type": "inputPageBlockList", "items": items }),
            index,
        )
    }

    fn strip_indent(line: &str, width: usize) -> &str {
        let mut stripped = 0;
        let mut cut = 0;
        for (offset, ch) in line.char_indices() {
            if stripped >= width || ch != ' ' {
                cut = offset;
                break;
            }
            stripped += 1;
            cut = offset + ch.len_utf8();
        }
        &line[cut..]
    }

    // ── 表格 ──────────────────────────────────────────────────────

    fn is_table_start(lines: &[&str], index: usize) -> bool {
        lines[index].contains('|')
            && lines
                .get(index + 1)
                .is_some_and(|next| is_table_delimiter(next))
    }

    fn is_table_delimiter(line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains('|') {
            return false;
        }
        let mut dashes = 0;
        for ch in trimmed.chars() {
            match ch {
                '-' => dashes += 1,
                '|' | ':' | ' ' => {}
                _ => return false,
            }
        }
        dashes > 0
    }

    fn split_row(line: &str) -> Vec<String> {
        let trimmed = line.trim();
        let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
        let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
        trimmed
            .split('|')
            .map(|cell| cell.trim().to_owned())
            .collect()
    }

    #[derive(Debug, Clone, Copy)]
    enum Align {
        Left,
        Center,
        Right,
    }

    impl Align {
        fn of(cell: &str) -> Self {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            match (left, right) {
                (true, true) => Self::Center,
                (false, true) => Self::Right,
                _ => Self::Left,
            }
        }

        fn type_name(self) -> &'static str {
            match self {
                Self::Left => "pageBlockHorizontalAlignmentLeft",
                Self::Center => "pageBlockHorizontalAlignmentCenter",
                Self::Right => "pageBlockHorizontalAlignmentRight",
            }
        }
    }

    fn table(lines: &[&str], start: usize) -> (Value, usize) {
        let header = split_row(lines[start]);
        let aligns: Vec<Align> = split_row(lines[start + 1])
            .iter()
            .map(|cell| Align::of(cell.as_str()))
            .collect();
        let width = header.len().max(aligns.len());

        let mut rows: Vec<Value> = vec![row_json(&header, &aligns, width, true)];
        let mut index = start + 2;
        while index < lines.len() {
            let line = lines[index];
            if line.trim().is_empty() || !line.contains('|') {
                break;
            }
            rows.push(row_json(&split_row(line), &aligns, width, false));
            index += 1;
        }

        (
            json!({
                "@type": "inputPageBlockTable",
                "caption": plain(""),
                "cells": rows,
                "is_bordered": true,
                "is_striped": false,
                "is_compact": false,
            }),
            index,
        )
    }

    fn row_json(cells: &[String], aligns: &[Align], width: usize, is_header: bool) -> Value {
        let filled = (0..width)
            .map(|column| {
                let text = cells.get(column).map(|cell| cell.as_str()).unwrap_or("");
                let align = aligns.get(column).copied().unwrap_or(Align::Left);
                json!({
                    "@type": "pageBlockTableCell",
                    "text": inline(text),
                    "is_header": is_header,
                    "colspan": 1,
                    "rowspan": 1,
                    "align": { "@type": align.type_name() },
                    "valign": { "@type": "pageBlockVerticalAlignmentTop" },
                })
            })
            .collect();
        Value::Array(filled)
    }

    // ── 行内 ──────────────────────────────────────────────────────

    /// 一段行内文本 → 一条 `RichText`（多段则包成 `richTexts`）。
    fn inline(text: &str) -> Value {
        join(parse_inline(text))
    }

    fn parse_inline(text: &str) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut buffer = String::new();
        let mut rest = text;

        while let Some(ch) = rest.chars().next() {
            if ch == '\\' {
                let mut chars = rest.char_indices();
                chars.next();
                match chars.next() {
                    Some((offset, escaped)) => {
                        buffer.push(escaped);
                        rest = &rest[offset + escaped.len_utf8()..];
                    }
                    None => {
                        buffer.push('\\');
                        rest = "";
                    }
                }
                continue;
            }
            // 内联 HTML（文档允许 md 里夹任意 HTML，且内联标签内的 md 会照常解析）
            if ch == '<' {
                if let Some((value, after)) = inline_html(rest) {
                    flush(&mut out, &mut buffer);
                    out.push(value);
                    rest = after;
                    continue;
                }
            }
            if ch == '`' {
                if let Some((inner, after)) = delimited(rest, "`") {
                    flush(&mut out, &mut buffer);
                    out.push(richtext::fixed(plain(inner)));
                    rest = after;
                    continue;
                }
            }
            // `$公式$`（`$$` 是块级，交给 parse_blocks）
            if ch == '$' && !rest.starts_with("$$") {
                if let Some((inner, after)) = delimited(rest, "$") {
                    flush(&mut out, &mut buffer);
                    out.push(richtext::mathematical_expression(inner));
                    rest = after;
                    continue;
                }
            }
            if let Some((inner, after)) = delimited(rest, "***").or_else(|| {
                if word_boundary(&buffer) {
                    delimited(rest, "___")
                } else {
                    None
                }
            }) {
                flush(&mut out, &mut buffer);
                out.push(richtext::bold(richtext::italic(inline(inner))));
                rest = after;
                continue;
            }
            if let Some((inner, after)) = delimited(rest, "**").or_else(|| {
                if word_boundary(&buffer) {
                    delimited(rest, "__")
                } else {
                    None
                }
            }) {
                flush(&mut out, &mut buffer);
                out.push(richtext::bold(inline(inner)));
                rest = after;
                continue;
            }
            if let Some((inner, after)) = delimited(rest, "~~") {
                flush(&mut out, &mut buffer);
                out.push(richtext::strikethrough(inline(inner)));
                rest = after;
                continue;
            }
            if let Some((inner, after)) = delimited(rest, "==") {
                flush(&mut out, &mut buffer);
                out.push(richtext::marked(inline(inner)));
                rest = after;
                continue;
            }
            if let Some((inner, after)) = delimited(rest, "||") {
                flush(&mut out, &mut buffer);
                out.push(richtext::spoiler(inline(inner)));
                rest = after;
                continue;
            }
            if let Some((inner, after)) = delimited(rest, "*").or_else(|| {
                // GFM：`_` 不在词内解析，`snake_case_name` 里没有斜体
                if word_boundary(&buffer) {
                    delimited(rest, "_")
                } else {
                    None
                }
            }) {
                flush(&mut out, &mut buffer);
                out.push(richtext::italic(inline(inner)));
                rest = after;
                continue;
            }
            // `![alt](tg://emoji?id=…)` / `![22:45 tomorrow](tg://time?…)`
            if ch == '!' {
                if let Some((value, after)) = inline_image(rest) {
                    flush(&mut out, &mut buffer);
                    out.push(value);
                    rest = after;
                    continue;
                }
            }
            if ch == '[' {
                // 脚注引用 `[^id]`
                if let Some((name, after)) = footnote_ref(rest) {
                    flush(&mut out, &mut buffer);
                    // 可见文字用引用名本身（`[^id1]` 里的 `id1`），不保留方括号语法。
                    // 角标编号是服务端渲染的事，本地算不出来也不该算。
                    out.push(richtext::reference_link(
                        plain(name),
                        name,
                        format!("#{name}"),
                    ));
                    rest = after;
                    continue;
                }
                if let Some((label, url, after)) = link(rest) {
                    flush(&mut out, &mut buffer);
                    out.push(link_value(label, url));
                    rest = after;
                    continue;
                }
            }
            buffer.push(ch);
            rest = &rest[ch.len_utf8()..];
        }

        flush(&mut out, &mut buffer);
        out
    }

    /// 链接按目标协议分派：`mailto:` / `tel:` / `tg://user?id=` / `#锚点` 各有专用表达式，
    /// 其余都是普通 `richTextUrl`（和 Bot API 的渲染规则一致）。
    fn link_value(label: &str, target: &str) -> Value {
        let text = inline(label);
        if let Some(address) = target.strip_prefix("mailto:") {
            return richtext::email_address(text, address);
        }
        if let Some(phone) = target.strip_prefix("tel:") {
            return richtext::phone_number(text, phone);
        }
        if let Some(id) = query_param(target, "tg://user", "id") {
            if let Ok(user_id) = id.parse::<i64>() {
                return richtext::mention_name(text, user_id);
            }
        }
        if let Some(anchor) = target.strip_prefix('#') {
            return richtext::anchor_link(text, anchor, target);
        }
        richtext::url(text, target)
    }

    /// 行内图片语法：只有 `tg://emoji` 与 `tg://time` 是**行内**表达式；
    /// `![](http…)` 是**块级媒体**，由 [`parse_blocks`] 处理，这里返回 `None` 保持原文。
    fn inline_image(text: &str) -> Option<(Value, &str)> {
        let inner = text.strip_prefix("![")?;
        let close = inner.find("](")?;
        let alt = &inner[..close];
        let tail = &inner[close + 2..];
        let end = tail.find(')')?;
        let target = tail[..end].trim();
        let after = &tail[end + 1..];

        if let Some(id) = query_param(target, "tg://emoji", "id") {
            return Some((richtext::custom_emoji(id.parse().ok()?, alt), after));
        }
        if target.starts_with("tg://time") {
            let unix: i32 = query_param(target, "tg://time", "unix")?.parse().ok()?;
            let format = query_param(target, "tg://time", "format").unwrap_or("wDT");
            return Some((
                richtext::date_time(plain(alt), unix, date_format(format)),
                after,
            ));
        }
        None
    }

    /// `[^id]`（不含 `[^id]: 定义` 的形式——那是块级脚注）。
    fn footnote_ref(text: &str) -> Option<(&str, &str)> {
        let inner = text.strip_prefix("[^")?;
        let close = inner.find(']')?;
        let name = &inner[..close];
        if name.is_empty() || name.contains(':') {
            return None;
        }
        Some((name, &inner[close + 1..]))
    }

    /// `tg://xxx?a=b` 形式的查询串取值。
    fn query_param<'a>(target: &'a str, scheme: &str, key: &str) -> Option<&'a str> {
        let rest = target.strip_prefix(scheme)?;
        let query = rest.strip_prefix('?')?;
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=')?;
            if name == key {
                return Some(value);
            }
        }
        None
    }

    /// Bot API 的 date-time 格式串（`wDT` 这种 ICU 风格）→ TDLib 的
    /// `DateTimeFormattingType`。只能近似：TDLib 只有「相对 / 绝对 + 两个精度 + 星期」。
    fn date_format(spec: &str) -> richtext::DateTimeFormat {
        use super::richtext::{DateTimeFormat, DateTimePrecision};
        if spec.is_empty() {
            return DateTimeFormat::Relative;
        }
        let time = if spec.contains("TT") {
            DateTimePrecision::Long
        } else if spec.contains('T') {
            DateTimePrecision::Short
        } else {
            DateTimePrecision::None
        };
        let date = if spec.contains("DD") {
            DateTimePrecision::Long
        } else if spec.contains('D') {
            DateTimePrecision::Short
        } else {
            DateTimePrecision::None
        };
        DateTimeFormat::Absolute {
            time,
            date,
            show_day_of_week: spec.contains('w'),
        }
    }

    /// 内联 HTML 标签（文档列出的那一小撮）。`<a>` 用 `href` 分派到对应链接表达式。
    /// 返回 `None` 表示「不认识 / 不是标签」，调用方按普通字符处理。
    fn inline_html(text: &str) -> Option<(Value, &str)> {
        let tag = parse_tag(text)?;
        if tag.self_closing {
            return None;
        }
        let (inner, after) = close_of(tag.rest, tag.name)?;
        let value = match tag.name.to_ascii_lowercase().as_str() {
            "b" | "strong" => richtext::bold(inline(inner)),
            "i" | "em" => richtext::italic(inline(inner)),
            "u" | "ins" => richtext::underline(inline(inner)),
            "s" | "strike" | "del" => richtext::strikethrough(inline(inner)),
            // 行内代码里不解析任何标记
            "code" => richtext::fixed(plain(inner)),
            "mark" => richtext::marked(inline(inner)),
            "sub" => richtext::subscript(inline(inner)),
            "sup" => richtext::superscript(inline(inner)),
            "tg-spoiler" => richtext::spoiler(inline(inner)),
            "tg-math" => richtext::mathematical_expression(inner),
            "tg-emoji" => richtext::custom_emoji(tag.attr("emoji-id")?.parse().ok()?, inner),
            "tg-time" => {
                let unix: i32 = tag.attr("unix")?.parse().ok()?;
                richtext::date_time(
                    inline(inner),
                    unix,
                    date_format(tag.attr("format").unwrap_or("")),
                )
            }
            "tg-reference" => richtext::reference(tag.attr("name")?, inline(inner)),
            "a" => link_value(inner, tag.attr("href")?),
            _ => return None,
        };
        Some((value, after))
    }

    /// `<tag attr="v">` 的头。`rest` 是 `>` 之后的部分。
    struct Tag<'a> {
        name: &'a str,
        attrs: Vec<(&'a str, &'a str)>,
        self_closing: bool,
        rest: &'a str,
    }

    impl<'a> Tag<'a> {
        fn attr(&self, key: &str) -> Option<&'a str> {
            self.attrs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
        }

        fn has(&self, key: &str) -> bool {
            self.attrs.iter().any(|(name, _)| *name == key)
        }
    }

    fn parse_tag(text: &str) -> Option<Tag<'_>> {
        let inner = text.strip_prefix('<')?;
        let end = inner.find('>')?;
        let body = inner[..end].trim();
        let rest = &inner[end + 1..];
        if body.is_empty()
            || body.starts_with('/')
            || body.starts_with('!')
            || body.starts_with('?')
        {
            return None;
        }
        let self_closing = body.ends_with('/');
        let body = body.strip_suffix('/').unwrap_or(body).trim_end();
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let name = &body[..name_end];
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return None;
        }

        let mut attrs: Vec<(&str, &str)> = Vec::new();
        let mut tail = body[name_end..].trim_start();
        while !tail.is_empty() {
            // key 到空白或 `=` 为止（只有空白会把 `src="…"` 整个当成 key）
            let key_end = tail
                .find(|ch: char| ch.is_whitespace() || ch == '=')
                .unwrap_or(tail.len());
            let key = &tail[..key_end];
            let mut remainder = tail[key_end..].trim_start();
            let mut value = "";
            if let Some(after_eq) = remainder.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                let (parsed, leftover) = match after_eq.chars().next() {
                    Some(quote @ ('"' | '\'')) => {
                        let quoted = &after_eq[1..];
                        match quoted.find(quote) {
                            Some(close) => (&quoted[..close], &quoted[close + 1..]),
                            None => (quoted, ""),
                        }
                    }
                    _ => {
                        let len = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
                        (&after_eq[..len], &after_eq[len..])
                    }
                };
                value = parsed;
                remainder = leftover.trim_start();
            }
            if key.is_empty() {
                break;
            }
            attrs.push((key, value));
            tail = remainder;
        }

        Some(Tag {
            name,
            attrs,
            self_closing,
            rest,
        })
    }

    /// `<tag>…</tag>` 的内容与之后的部分。
    fn close_of<'a>(inner: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
        let marker = format!("</{name}>");
        let end = inner.find(&marker)?;
        Some((&inner[..end], &inner[end + marker.len()..]))
    }

    /// GFM：`_` / `__` / `___` **不在词内**解析（`snake_case_name` 里没有斜体）。
    ///
    /// 判断方式是看已缓冲文本的最后一个字符——是字母数字就说明标记长在词中间。
    fn word_boundary(buffer: &str) -> bool {
        buffer.chars().last().is_none_or(|ch| !ch.is_alphanumeric())
    }

    /// `marker … marker`；找不到闭合、或内容为空时返回 `None`（按字面处理，也避免死循环）。
    fn delimited<'a>(text: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
        let inner = text.strip_prefix(marker)?;
        let end = inner.find(marker)?;
        if end == 0 {
            return None;
        }
        Some((&inner[..end], &inner[end + marker.len()..]))
    }

    fn link(text: &str) -> Option<(&str, &str, &str)> {
        let inner = text.strip_prefix('[')?;
        let close = inner.find("](")?;
        let label = &inner[..close];
        let tail = &inner[close + 2..];
        let end = tail.find(')')?;
        let url = tail[..end].trim();
        if url.is_empty() {
            return None;
        }
        Some((label, url, &tail[end + 1..]))
    }

    fn flush(out: &mut Vec<Value>, buffer: &mut String) {
        if !buffer.is_empty() {
            out.push(plain(buffer.as_str()));
            buffer.clear();
        }
    }

    fn join(mut parts: Vec<Value>) -> Value {
        match parts.len() {
            0 => plain(""),
            1 => parts.remove(0),
            _ => json!({ "@type": "richTexts", "texts": parts }),
        }
    }

    // ── 构造器 ────────────────────────────────────────────────────

    fn paragraph_block(text: &str) -> Value {
        json!({ "@type": "inputPageBlockParagraph", "text": inline(text) })
    }

    fn section_heading(text: &str, level: i32) -> Value {
        json!({
            "@type": "inputPageBlockSectionHeading",
            "text": inline(text),
            "size": level.clamp(1, 6),
        })
    }

    fn preformatted(text: &str, language: &str) -> Value {
        json!({
            "@type": "inputPageBlockPreformatted",
            "text": plain(text),
            "language": language,
        })
    }

    fn list_item(blocks: Vec<Value>, checkbox: Option<bool>, value: i32) -> Value {
        json!({
            "@type": "inputPageBlockListItem",
            "blocks": blocks,
            "has_checkbox": checkbox.is_some(),
            "is_checked": checkbox.unwrap_or(false),
            "value": value,
            "type": if value > 0 { "1" } else { "" },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(markdown: &str) -> Vec<Value> {
        markdown::to_blocks(markdown)
    }

    #[test]
    fn headings_and_paragraphs_keep_their_levels() {
        let blocks = blocks("# 一级\n\n正文第一行\n正文第二行\n\n### 三级");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["@type"], "inputPageBlockSectionHeading");
        assert_eq!(blocks[0]["size"], 1);
        assert_eq!(blocks[0]["text"]["text"], "一级");
        assert_eq!(blocks[1]["@type"], "inputPageBlockParagraph");
        // 段内换行保留
        assert_eq!(blocks[1]["text"]["text"], "正文第一行\n正文第二行");
        assert_eq!(blocks[2]["size"], 3);
    }

    #[test]
    fn fenced_code_becomes_preformatted_with_language() {
        let blocks = blocks("```rust\nfn main() {}\n```");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["@type"], "inputPageBlockPreformatted");
        assert_eq!(blocks[0]["language"], "rust");
        assert_eq!(blocks[0]["text"]["text"], "fn main() {}");
        assert_eq!(blocks[0]["text"]["@type"], "richTextPlain");
    }

    #[test]
    fn lists_nest_and_support_checkboxes() {
        let blocks = blocks("- 一\n- [x] 二\n  - 二·一\n1. 甲\n2. 乙");
        assert_eq!(blocks.len(), 2, "无序与有序是两个列表块");
        let bullet = &blocks[0];
        assert_eq!(bullet["@type"], "inputPageBlockList");
        assert_eq!(bullet["items"].as_array().unwrap().len(), 2);
        assert_eq!(bullet["items"][1]["has_checkbox"], true);
        assert_eq!(bullet["items"][1]["is_checked"], true);
        assert_eq!(bullet["items"][0]["value"], 0);
        // 嵌套列表在第二项的 blocks 里
        let nested = bullet["items"][1]["blocks"].as_array().unwrap();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[1]["@type"], "inputPageBlockList");
        assert_eq!(nested[1]["items"][0]["blocks"][0]["text"]["text"], "二·一");

        let ordered = &blocks[1];
        assert_eq!(ordered["items"][0]["value"], 1);
        assert_eq!(ordered["items"][1]["value"], 2);
        assert_eq!(ordered["items"][0]["type"], "1");
    }

    #[test]
    fn quotes_divider_and_math_are_recognised() {
        let blocks = blocks("> 引用一行\n> 引用两行\n\n---\n\n$$\na^2+b^2\n$$");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["@type"], "inputPageBlockBlockQuote");
        assert!(blocks[0]["credit"].is_null(), "引用没有 credit");
        assert_eq!(blocks[0]["blocks"][0]["text"]["text"], "引用一行\n引用两行");
        assert_eq!(blocks[1]["@type"], "inputPageBlockDivider");
        assert_eq!(blocks[2]["@type"], "inputPageBlockMathematicalExpression");
        assert_eq!(blocks[2]["expression"], "a^2+b^2");
    }

    #[test]
    fn tables_carry_alignment_and_header_flags() {
        let blocks = blocks("| 名 | 值 |\n|:---|---:|\n| a | 1 |\n| b | 2 |");
        assert_eq!(blocks.len(), 1);
        let table = &blocks[0];
        assert_eq!(table["@type"], "inputPageBlockTable");
        let rows = table["cells"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "表头 + 两行");
        assert_eq!(rows[0][0]["is_header"], true);
        assert_eq!(
            rows[0][0]["align"]["@type"],
            "pageBlockHorizontalAlignmentLeft"
        );
        assert_eq!(
            rows[1][1]["align"]["@type"],
            "pageBlockHorizontalAlignmentRight"
        );
        assert_eq!(rows[2][0]["text"]["text"], "b");
        assert_eq!(
            rows[2][0]["valign"]["@type"],
            "pageBlockVerticalAlignmentTop"
        );
    }

    #[test]
    fn inline_styles_links_and_escapes() {
        let blocks =
            blocks("**粗** 与 *斜* 与 ~~删~~ 与 `码` 与 [文字](https://example.com) 与 \\*字面\\*");
        let text = &blocks[0]["text"];
        assert_eq!(text["@type"], "richTexts");
        let parts = text["texts"].as_array().unwrap();
        let kinds: Vec<&str> = parts
            .iter()
            .map(|part| part["@type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "richTextBold",
                "richTextPlain",
                "richTextItalic",
                "richTextPlain",
                "richTextStrikethrough",
                "richTextPlain",
                "richTextFixed",
                "richTextPlain",
                "richTextUrl",
                "richTextPlain",
            ]
        );
        let link = parts
            .iter()
            .find(|part| part["@type"] == "richTextUrl")
            .expect("应有链接");
        assert_eq!(link["url"], "https://example.com");
        assert_eq!(link["text"]["text"], "文字");
        // 转义后是字面星号
        let last = parts.last().unwrap();
        assert!(last["text"].as_str().unwrap().contains("*字面*"));
    }

    #[test]
    fn nested_emphasis_and_unclosed_markers_stay_literal() {
        let blocks = blocks("***又粗又斜*** 与 **没闭合");
        let parts = blocks[0]["text"]["texts"].as_array().unwrap().to_vec();
        assert_eq!(parts[0]["@type"], "richTextBold");
        assert_eq!(parts[0]["text"]["@type"], "richTextItalic");
        assert_eq!(parts[0]["text"]["text"]["text"], "又粗又斜");
        let literal: String = parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect();
        assert!(literal.contains("**没闭合"), "没闭合的标记原样留着");
    }

    /// GFM：`_` **不在词内**解析，`snake_case_name` 不该变成斜体。
    #[test]
    fn underscores_inside_words_stay_literal() {
        let blocks = blocks("变量 snake_case_name 与 _真的斜体_");
        let parts = blocks[0]["text"]["texts"].as_array().unwrap();
        let joined: String = parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect();
        assert!(
            joined.contains("snake_case_name"),
            "词内下划线应原样保留：{joined}"
        );
        assert_eq!(
            parts
                .iter()
                .filter(|part| part["@type"] == "richTextItalic")
                .count(),
            1,
            "只有行首那对 `_` 是斜体"
        );
    }

    #[test]
    fn unsupported_media_syntax_stays_text() {
        let blocks = blocks("![图](/tmp/a.jpg)");
        assert_eq!(blocks[0]["@type"], "inputPageBlockParagraph");
        let parts = blocks[0]["text"]["texts"].as_array().unwrap();
        assert_eq!(parts[1]["@type"], "richTextUrl", "只当链接，不当图片");
    }

    #[test]
    fn empty_markdown_has_no_blocks_and_is_refused() {
        assert!(blocks("   \n\n").is_empty());
        assert!(RichMessage::from_markdown("").is_empty());
        assert!(
            !RichMessage::from_markdown("正文").is_empty(),
            "有内容就不是空消息"
        );
    }

    #[test]
    fn message_json_has_the_three_source_shapes() {
        let local = RichMessage::from_markdown("# 标题").to_json();
        assert_eq!(local["@type"], "inputRichMessage");
        assert_eq!(local["source"]["@type"], "richMessageSourceBlocks");
        assert_eq!(local["detect_automatic_blocks"], true, "默认开启自动识别");
        assert_eq!(local["is_rtl"], false);

        let server = RichMessage::server_markdown("# 标题")
            .with_media(vec![RichMedia::new("cover", Media::photo("/tmp/a.jpg"))])
            .rtl(true);
        let object = server.to_json();
        assert_eq!(object["source"]["@type"], "richMessageSourceMarkdown");
        assert_eq!(object["source"]["text"], "# 标题");
        assert_eq!(
            object["source"]["media"][0]["@type"],
            "inputRichMessageMedia"
        );
        assert_eq!(object["source"]["media"][0]["id"], "cover");
        // 媒体沿用 content_json 的形状：inputMessagePhoto → inputPhoto → InputFile
        assert_eq!(
            object["source"]["media"][0]["media"]["photo"]["photo"]["@type"],
            "inputFileLocal"
        );
        assert_eq!(object["is_rtl"], true);

        let html = RichMessage::server_html("<b>粗</b>").to_json();
        assert_eq!(html["source"]["@type"], "richMessageSourceHtml");

        let content = rich_content_json(&RichMessage::from_markdown("正文"), true);
        assert_eq!(content["@type"], "inputMessageRichMessage");
        assert_eq!(content["clear_draft"], true);
        assert_eq!(
            content["message"]["source"]["@type"],
            "richMessageSourceBlocks"
        );
    }

    #[test]
    fn button_rows_carry_style_and_action() {
        let message = RichMessage::from_markdown("正文")
            .with_button_row(
                vec![
                    RichButton::url("看文档", "https://example.com")
                        .style(RichButtonStyle::Primary),
                    RichButton::copy_text("复制", "hello"),
                ],
                Some(RichButtonAlign::Center),
            )
            .unwrap();
        let blocks = message.to_json()["source"]["blocks"].clone();
        let row = blocks.as_array().unwrap().last().unwrap();
        assert_eq!(row["@type"], "inputPageBlockButtonRow");
        assert_eq!(
            row["align"]["@type"], "pageBlockHorizontalAlignmentCenter",
            "显式对齐"
        );
        assert_eq!(row["buttons"].as_array().unwrap().len(), 2);

        let url = &row["buttons"][0];
        assert_eq!(url["@type"], "inlineButton");
        // 按钮文字是 RichText，但只允许 plain / texts / customEmoji——这里固定 plain
        assert_eq!(url["text"]["@type"], "richTextPlain");
        assert_eq!(url["text"]["text"], "看文档");
        assert_eq!(url["style"]["@type"], "buttonStylePrimary", "style 必填");
        assert_eq!(url["type"]["@type"], "inlineKeyboardButtonTypeUrl");
        assert_eq!(url["type"]["url"], "https://example.com");

        let copy = &row["buttons"][1];
        assert_eq!(copy["style"]["@type"], "buttonStyleDefault");
        assert_eq!(copy["type"]["@type"], "inlineKeyboardButtonTypeCopyText");
        assert_eq!(copy["type"]["text"], "hello");

        // 铺满整行 → align 为 null（TDLib 语义：null 才是全宽）
        let wide = RichMessage::from_markdown("正文")
            .with_buttons(vec![RichButton::user("作者", 42)])
            .unwrap()
            .to_json();
        let wide_row = wide["source"]["blocks"].as_array().unwrap().last().unwrap();
        assert!(wide_row["align"].is_null(), "不传对齐就是铺满整行");
        assert_eq!(
            wide_row["buttons"][0]["type"]["@type"],
            "inlineKeyboardButtonTypeUser"
        );
        assert_eq!(wide_row["buttons"][0]["type"]["user_id"], 42);

        // callback 的 data 是 bytes → base64，和普通 inline 键盘同一套编码
        let called = RichButton::callback("点我", "hello").to_json();
        assert_eq!(called["type"]["@type"], "inlineKeyboardButtonTypeCallback");
        assert_eq!(called["type"]["data"], "aGVsbG8=");
    }

    /// 按钮行只能在 Blocks 源上：md / html 源的块树在服务端，本地插不进去。
    #[test]
    fn button_rows_are_refused_on_server_parsed_sources() {
        let buttons = vec![RichButton::url("x", "https://example.com")];
        assert!(matches!(
            RichMessage::server_markdown("*hi*").with_buttons(buttons.clone()),
            Err(SimplifyError::RichSourceNotBlocks)
        ));
        assert!(matches!(
            RichMessage::server_html("<b>x</b>").with_buttons(buttons),
            Err(SimplifyError::RichSourceNotBlocks)
        ));
        // 空按钮行本地就挡掉，不白跑一次往返
        assert!(matches!(
            RichMessage::from_markdown("正文").with_buttons(Vec::new()),
            Err(SimplifyError::EmptyButtonRow)
        ));
        // 空按钮 ≠ 报错：等于没按钮
        let params = RichMessageParams::markdown_with_buttons("正文", Vec::new());
        assert_eq!(
            params.message.to_json()["source"]["blocks"][0]["@type"],
            "inputPageBlockParagraph"
        );
    }

    /// 我们产出的**每一块**都要能被 `td_api::InputPageBlock` 反序列化。
    ///
    /// 这比「构造器名存在于 schema」强一层：字段名拼错、类型不对（`i32` 给成字符串、
    /// `bytes` 没 base64）都会在这里炸，而 `check_request` 只看顶层参数名，管不到嵌套结构。
    #[test]
    fn every_block_we_emit_deserializes_into_td_api() {
        let markdown = "# 一级\n\n正文 **粗** [链](https://x)\n\n```rust\nfn main() {}\n```\n\n\
                        > 引用\n\n- 一\n- [x] 二\n\n---\n\n$$\na^2\n$$\n\n\
                        | a | b |\n|:--|--:|\n| 1 | 2 |\n";
        let mut tops = markdown::to_blocks(markdown);
        // 列表项不是顶层块，单独校验（`inputPageBlockListItem` 属于 ListItem 类）
        let list = tops
            .iter()
            .find(|block| block["@type"] == "inputPageBlockList")
            .expect("样例里应有列表");
        for item in list["items"].as_array().unwrap() {
            serde_json::from_value::<crate::td_api::InputPageBlockListItem>(item.clone())
                .unwrap_or_else(|error| panic!("listItem 反序列化失败（{error}）：{item}"));
        }
        let message = RichMessage::from_blocks(tops.clone())
            .with_button_row(
                vec![
                    RichButton::url("a", "https://a").style(RichButtonStyle::Success),
                    RichButton::callback("b", "data"),
                    RichButton::copy_text("c", "copied"),
                    RichButton::user("d", 7),
                ],
                Some(RichButtonAlign::End),
            )
            .unwrap();
        tops.push(
            message.to_json()["source"]["blocks"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .clone(),
        );

        for block in &tops {
            serde_json::from_value::<crate::td_api::InputPageBlock>(block.clone())
                .unwrap_or_else(|error| panic!("块反序列化失败（{error}）：{block}"));
        }
        assert!(
            tops.len() >= 9,
            "样例应覆盖 9 类块，实际只有 {}",
            tops.len()
        );
    }

    /// `RichText` 的 **31 个表达式**：每一个都构造一次、命名一次、反序列化一次。
    ///
    /// 这比「md 转出来的那几个能用」强得多：`richtext::*` 是新公开面，
    /// 字段名 / 类型 / 必填项一旦和真 schema 不一致，这里立刻红。
    #[test]
    fn all_31_rich_text_expressions_build_and_deserialize() {
        use super::richtext::{self, DateTimeFormat, DateTimePrecision};

        // `richTextIcon` 要一个真实的 `document` 对象（不是 inputDocument）
        let document = json!({
            "@type": "document",
            "file_name": "a.png",
            "mime_type": "image/png",
            "document": { "@type": "file", "id": 1, "size": 1, "expected_size": 1 },
        });
        let plain = || richtext::plain("x");

        let cases: Vec<(&str, Value)> = vec![
            ("richTextPlain", richtext::plain("纯文本")),
            ("richTextBold", richtext::bold(plain())),
            ("richTextItalic", richtext::italic(plain())),
            ("richTextUnderline", richtext::underline(plain())),
            ("richTextStrikethrough", richtext::strikethrough(plain())),
            ("richTextSpoiler", richtext::spoiler(plain())),
            ("richTextSubscript", richtext::subscript(plain())),
            ("richTextSuperscript", richtext::superscript(plain())),
            ("richTextMarked", richtext::marked(plain())),
            (
                "richTextDateTime",
                richtext::date_time(
                    plain(),
                    1_700_000_000,
                    DateTimeFormat::Absolute {
                        time: DateTimePrecision::Short,
                        date: DateTimePrecision::Long,
                        show_day_of_week: true,
                    },
                ),
            ),
            ("richTextMention", richtext::mention(plain(), "durov")),
            ("richTextHashtag", richtext::hashtag(plain(), "#rust")),
            ("richTextCashtag", richtext::cashtag(plain(), "$USD")),
            (
                "richTextBankCardNumber",
                richtext::bank_card_number(plain(), "4242 4242 4242 4242"),
            ),
            (
                "richTextTonAddress",
                richtext::ton_address(plain(), "EQAbc…"),
            ),
            (
                "richTextBotCommand",
                richtext::bot_command(plain(), "/start"),
            ),
            ("richTextFixed", richtext::fixed(plain())),
            ("richTextMentionName", richtext::mention_name(plain(), 42)),
            ("richTextUrl", richtext::url(plain(), "https://example.com")),
            (
                "richTextEmailAddress",
                richtext::email_address(plain(), "a@example.com"),
            ),
            (
                "richTextPhoneNumber",
                richtext::phone_number(plain(), "+8613800000000"),
            ),
            (
                "richTextCustomEmoji",
                richtext::custom_emoji(5368324170671202286, "😀"),
            ),
            ("richTextIcon", richtext::icon(document, 16, 16)),
            (
                "richTextMathematicalExpression",
                richtext::mathematical_expression("a^2+b^2"),
            ),
            (
                "richTextButton",
                richtext::button(&RichButton::url("按钮", "https://example.com")),
            ),
            (
                "richTextDiff",
                richtext::diff(plain(), richtext::plain("旧")),
            ),
            ("richTextReference", richtext::reference("1", plain())),
            (
                "richTextReferenceLink",
                richtext::reference_link(plain(), "1", "https://example.com#ref1"),
            ),
            ("richTextAnchor", richtext::anchor("sec1")),
            (
                "richTextAnchorLink",
                richtext::anchor_link(plain(), "sec1", "https://example.com#sec1"),
            ),
            (
                "richTexts",
                richtext::concat(vec![plain(), richtext::bold(plain())]),
            ),
        ];

        assert_eq!(cases.len(), 31, "TDLib 的 RichText 一共 31 个构造器");
        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        let mut seen: Vec<&str> = cases.iter().map(|(name, _)| *name).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 31, "31 个名字不能有重复");

        for (name, value) in &cases {
            assert_eq!(value["@type"].as_str(), Some(*name), "{name} 的 @type 不对");
            assert!(schema.get(name).is_some(), "真 schema 里没有 {name}");
            serde_json::from_value::<crate::td_api::RichText>(value.clone())
                .unwrap_or_else(|error| panic!("{name} 反序列化失败（{error}）：{value}"));
        }
    }

    /// 表达式要能真的塞进块里——块级反序列化会连 `text` 一起校验。
    #[test]
    fn rich_text_expressions_fit_inside_blocks() {
        use super::richtext;

        let text = richtext::concat(vec![
            richtext::bold(richtext::plain("粗")),
            richtext::plain(" + "),
            richtext::spoiler(richtext::plain("剧透")),
            richtext::plain(" + "),
            richtext::underline(richtext::custom_emoji(1, "😀")),
        ]);
        let block = json!({ "@type": "inputPageBlockParagraph", "text": text });
        serde_json::from_value::<crate::td_api::InputPageBlock>(block.clone())
            .unwrap_or_else(|error| panic!("段落块反序列化失败（{error}）：{block}"));

        // 块级锚点 + 文本里的锚点链接，成对出现才有意义
        let anchor_block = json!({ "@type": "inputPageBlockAnchor", "name": "sec1" });
        serde_json::from_value::<crate::td_api::InputPageBlock>(anchor_block.clone())
            .unwrap_or_else(|error| panic!("锚点块反序列化失败（{error}）：{anchor_block}"));

        // 块标题同样是 RichText
        let heading = json!({
            "@type": "inputPageBlockSectionHeading",
            "text": richtext::button(&RichButton::copy_text("复制", "x")),
            "size": 2,
        });
        serde_json::from_value::<crate::td_api::InputPageBlock>(heading.clone())
            .unwrap_or_else(|error| panic!("标题块反序列化失败（{error}）：{heading}"));
    }

    /// 官方「Rich Markdown」表里的**行内**语法，逐项验一遍。
    #[test]
    fn markdown_covers_the_official_inline_syntax() {
        let blocks = blocks(
            "**粗** __粗__ *斜* _斜_ ~~删~~ `码` ==标记== ||剧透|| $x^2$ \
             [链接](https://a.b) [邮箱](mailto:a@b.c) [电话](tel:+123) [人](tg://user?id=42) [跳](#sec)",
        );
        let parts = blocks[0]["text"]["texts"].as_array().unwrap();
        let kinds: Vec<&str> = parts
            .iter()
            .map(|part| part["@type"].as_str().unwrap())
            .collect();
        for expected in [
            "richTextBold",
            "richTextItalic",
            "richTextStrikethrough",
            "richTextFixed",
            "richTextMarked",
            "richTextSpoiler",
            "richTextMathematicalExpression",
            "richTextUrl",
            "richTextEmailAddress",
            "richTextPhoneNumber",
            "richTextMentionName",
            "richTextAnchorLink",
        ] {
            assert!(kinds.contains(&expected), "缺 {expected}：{kinds:?}");
        }

        // 目标协议决定表达式，字段必须真的解析出来
        let email = parts
            .iter()
            .find(|part| part["@type"] == "richTextEmailAddress")
            .unwrap();
        assert_eq!(email["email_address"], "a@b.c");
        let phone = parts
            .iter()
            .find(|part| part["@type"] == "richTextPhoneNumber")
            .unwrap();
        assert_eq!(phone["phone_number"], "+123");
        let mention = parts
            .iter()
            .find(|part| part["@type"] == "richTextMentionName")
            .unwrap();
        assert_eq!(mention["user_id"], 42);
        let anchor = parts
            .iter()
            .find(|part| part["@type"] == "richTextAnchorLink")
            .unwrap();
        assert_eq!(anchor["anchor_name"], "sec");
    }

    /// 行内自定义 emoji / 时间，以及文档里出现的那批内联 HTML 标签。
    #[test]
    fn inline_media_and_html_tags_are_understood() {
        let blocks = blocks(
            "![👍](tg://emoji?id=5368324170671202286) ![明天](tg://time?unix=1647531900&format=wDT) \
             <u>下划线</u> <sub>2</sub> <sup>3</sup> <mark>高亮</mark> <tg-spoiler>剧透</tg-spoiler> \
             <code>x</code> <b>粗</b> <tg-emoji emoji-id=\"7\">😀</tg-emoji> \
             <a href=\"mailto:a@b.c\">信</a>",
        );
        let parts = blocks[0]["text"]["texts"].as_array().unwrap();
        let kinds: Vec<&str> = parts
            .iter()
            .map(|part| part["@type"].as_str().unwrap())
            .collect();
        for expected in [
            "richTextCustomEmoji",
            "richTextDateTime",
            "richTextUnderline",
            "richTextSubscript",
            "richTextSuperscript",
            "richTextMarked",
            "richTextSpoiler",
            "richTextFixed",
            "richTextBold",
            "richTextEmailAddress",
        ] {
            assert!(kinds.contains(&expected), "缺 {expected}：{kinds:?}");
        }

        let emoji = parts
            .iter()
            .find(|part| part["@type"] == "richTextCustomEmoji")
            .unwrap();
        assert_eq!(emoji["custom_emoji_id"], 5368324170671202286_i64);
        assert_eq!(emoji["alternative_text"], "👍");
        let time = parts
            .iter()
            .find(|part| part["@type"] == "richTextDateTime")
            .unwrap();
        assert_eq!(time["unix_time"], 1_647_531_900);
        assert_eq!(
            time["formatting_type"]["@type"],
            "dateTimeFormattingTypeAbsolute"
        );
        assert_eq!(
            time["formatting_type"]["time_precision"]["@type"],
            "dateTimePartPrecisionShort"
        );
        assert_eq!(
            time["formatting_type"]["date_precision"]["@type"], "dateTimePartPrecisionShort",
            "wDT：D 是短日期（DD 才是长）"
        );
        assert_eq!(time["formatting_type"]["show_day_of_week"], true);
    }

    /// 官方「Rich Markdown / Rich HTML」里的**块级**语法，一类一个用例。
    #[test]
    fn block_syntax_covers_the_official_table() {
        let cases: [(&str, &str); 32] = [
            ("# 一级", "inputPageBlockSectionHeading"),
            ("<h3>html 标题</h3>", "inputPageBlockSectionHeading"),
            ("段落", "inputPageBlockParagraph"),
            ("<p>html 段落</p>", "inputPageBlockParagraph"),
            ("```rust\nfn a() {}\n```", "inputPageBlockPreformatted"),
            (
                "<pre><code class=\"language-rust\">fn a() {}</code></pre>",
                "inputPageBlockPreformatted",
            ),
            ("<footer>页脚</footer>", "inputPageBlockFooter"),
            ("---", "inputPageBlockDivider"),
            ("<hr/>", "inputPageBlockDivider"),
            ("- 项", "inputPageBlockList"),
            ("> 引用", "inputPageBlockBlockQuote"),
            ("<blockquote>引用</blockquote>", "inputPageBlockBlockQuote"),
            (
                "<blockquote expandable>折叠引用</blockquote>",
                "inputPageBlockExpandableBlockQuote",
            ),
            (
                "<aside>拉引<cite>作者</cite></aside>",
                "inputPageBlockPullQuote",
            ),
            ("| a |\n|:--|\n| 1 |", "inputPageBlockTable"),
            ("$$\nE=mc^2\n$$", "inputPageBlockMathematicalExpression"),
            (
                "```math\nE=mc^2\n```",
                "inputPageBlockMathematicalExpression",
            ),
            (
                "<tg-math-block>E=mc^2</tg-math-block>",
                "inputPageBlockMathematicalExpression",
            ),
            (
                "<details><summary>标题</summary>正文</details>",
                "inputPageBlockDetails",
            ),
            ("<a name=\"sec\"></a>", "inputPageBlockAnchor"),
            (
                "<tg-map lat=\"1\" long=\"2\" zoom=\"10\"/>",
                "inputPageBlockMap",
            ),
            ("![](https://x/a.jpg \"图注\")", "inputPageBlockPhoto"),
            ("![](https://x/a.mp4)", "inputPageBlockVideo"),
            ("![](https://x/a.gif)", "inputPageBlockAnimation"),
            ("![](https://x/a.mp3)", "inputPageBlockAudio"),
            ("![](https://x/a.ogg)", "inputPageBlockVoiceNote"),
            ("![](https://x/a.zip)", "inputPageBlockDocument"),
            (
                "<figure><img src=\"https://x/a.jpg\"/></figure>",
                "inputPageBlockPhoto",
            ),
            (
                "<tg-collage><img src=\"https://x/a.jpg\"/><img src=\"https://x/b.jpg\"/></tg-collage>",
                "inputPageBlockCollage",
            ),
            (
                "<tg-slideshow><img src=\"https://x/a.jpg\"/></tg-slideshow>",
                "inputPageBlockSlideshow",
            ),
            (
                "<tg-button-row><tg-button type=\"url\" url=\"https://t.me\">按钮</tg-button></tg-button-row>",
                "inputPageBlockButtonRow",
            ),
            (
                "<tg-thinking>思考中</tg-thinking>",
                "inputPageBlockThinking",
            ),
        ];
        for (markdown, expected) in cases {
            let blocks = blocks(markdown);
            assert_eq!(
                blocks.len(),
                1,
                "「{markdown}」应该只产出一个块：{blocks:?}"
            );
            assert_eq!(blocks[0]["@type"], expected, "输入：{markdown}");
            serde_json::from_value::<crate::td_api::InputPageBlock>(blocks[0].clone())
                .unwrap_or_else(|error| {
                    panic!("{expected} 反序列化失败（{error}）：{}", blocks[0])
                });
        }
    }

    /// 把官方那份「Nested Syntax Report」整段转出来，逐块反序列化。
    ///
    /// 这条是「全格式」的总闸：块类型、字段名、嵌套 RichText 一起过真 schema。
    #[test]
    fn official_sample_fully_deserializes() {
        let markdown = "# 报告 _Q1_\n\nIntro with <u>下划线</u>, ==标记==, and $x^2$.\n\n\
<details open><summary>摘要 **粗**</summary>\n\n### 内部标题\n- 项 _斜_\n\n</details>\n\n\
![](https://telegram.org/a.jpg \"图注\")\n\n\
| H1 | H2 |\n|:--|--:|\n| a | b |\n\n\
正文带脚注[^id1]。\n\n[^id1]: 脚注定义。\n\n\
<tg-collage><img src=\"https://x/a.jpg\"/><video src=\"https://x/b.mp4\"></video></tg-collage>\n\n\
<aside>拉引<cite>作者</cite></aside>\n\n\
<blockquote expandable>折叠<br>第二行<cite>我</cite></blockquote>\n\n\
<a name=\"ch1\"></a>\n\n<footer>页脚</footer>\n\n\
<tg-map lat=\"41.9\" long=\"12.5\" zoom=\"14\"/>\n\n\
<figure><img src=\"https://x/p.jpg\" tg-spoiler/><figcaption>说明<cite>署名</cite></figcaption></figure>\n\n\
<tg-button-row align=\"left\"><tg-button type=\"url\" url=\"https://t.me\">链接</tg-button><tg-button type=\"callback_data\" style=\"link\" data=\"cb\">回调</tg-button></tg-button-row>\n\n\
<pre><code class=\"language-python\">print(1)</code></pre>\n\n\
```math\nE=mc^2\n```\n";
        let blocks = markdown::to_blocks(markdown);
        assert_eq!(blocks.len(), 17, "样例块数不对：{}", blocks.len());
        for block in &blocks {
            serde_json::from_value::<crate::td_api::InputPageBlock>(block.clone())
                .unwrap_or_else(|error| panic!("反序列化失败（{error}）：{block}"));
        }
        // 几个关键块得真的在
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|block| block["@type"].as_str().unwrap())
            .collect();
        for expected in [
            "inputPageBlockDetails",
            "inputPageBlockCollage",
            "inputPageBlockPullQuote",
            "inputPageBlockExpandableBlockQuote",
            "inputPageBlockAnchor",
            "inputPageBlockFooter",
            "inputPageBlockMap",
            "inputPageBlockPhoto",
            "inputPageBlockButtonRow",
            "inputPageBlockPreformatted",
            "inputPageBlockMathematicalExpression",
        ] {
            assert!(kinds.contains(&expected), "缺 {expected}：{kinds:?}");
        }
    }

    /// Markdown 只产出这些表达式，其余必须走 `richtext::*`。
    ///
    /// 这条是「能力边界」的闭环：遍历 md 产出的**全部 JSON**，收集出现的每个
    /// `richText*` 构造器名，只要有一个不在白名单里就失败——防止以后给解析器加语法时
    /// 悄悄用到没测试过的表达式。
    #[test]
    fn markdown_rich_text_forms_are_all_in_the_documented_set() {
        // md 能表达的 21 个（剩下 10 个只能 `richtext::*`：mention / hashtag / cashtag /
        // bankCardNumber / tonAddress / botCommand / icon / button / diff / anchor）
        const FROM_MARKDOWN: [&str; 21] = [
            "richTextPlain",
            "richTexts",
            "richTextBold",
            "richTextItalic",
            "richTextUnderline",
            "richTextStrikethrough",
            "richTextSpoiler",
            "richTextSubscript",
            "richTextSuperscript",
            "richTextMarked",
            "richTextFixed",
            "richTextDateTime",
            "richTextMentionName",
            "richTextUrl",
            "richTextEmailAddress",
            "richTextPhoneNumber",
            "richTextCustomEmoji",
            "richTextMathematicalExpression",
            "richTextReference",
            "richTextReferenceLink",
            "richTextAnchorLink",
        ];

        let markdown = "# 标题 **粗** *斜* ~~删~~ `码` ==标记== ||剧透|| $x^2$ <u>下划线</u> <sub>2</sub>\n\n\
                        - [x] 项\n\n> 引用\n\n$$\na^2\n$$\n\n\
                        | a | b |\n|:--|--:|\n| 1 | 2 |\n\n\
                        [^n] 脚注\n\n[^n]: 定义\n\n\
                        [链](https://x) [邮](mailto:a@b) [话](tel:+1) [人](tg://user?id=1) [跳](#a)\n\n\
                        ![😀](tg://emoji?id=1) ![时](tg://time?unix=1&format=wDT)";
        let blocks = markdown::to_blocks(markdown);
        let mut found = std::collections::BTreeSet::new();
        collect_rich_text_types(&Value::Array(blocks), &mut found);
        for name in &found {
            assert!(
                FROM_MARKDOWN.contains(&name.as_str()),
                "md 产出了白名单之外的 {name}"
            );
        }
        // 样例里该出现的都得出现，否则「没出现」没意义
        for name in [
            "richTextPlain",
            "richTextBold",
            "richTextItalic",
            "richTextUnderline",
            "richTextStrikethrough",
            "richTextSpoiler",
            "richTextSubscript",
            "richTextMarked",
            "richTextFixed",
            "richTextDateTime",
            "richTextMentionName",
            "richTextUrl",
            "richTextEmailAddress",
            "richTextPhoneNumber",
            "richTextCustomEmoji",
            "richTextMathematicalExpression",
            "richTextReference",
            "richTextReferenceLink",
            "richTextAnchorLink",
        ] {
            assert!(found.contains(name), "样例里应该有 {name}");
        }
        assert_eq!(
            FROM_MARKDOWN.len(),
            21,
            "31 个表达式里 md 覆盖 21 个，其余 10 个走 richtext::*"
        );
    }

    fn collect_rich_text_types(value: &Value, out: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(kind) = map.get("@type").and_then(Value::as_str) {
                    if kind.starts_with("richText") {
                        out.insert(kind.to_owned());
                    }
                }
                for child in map.values() {
                    collect_rich_text_types(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_rich_text_types(item, out);
                }
            }
            _ => {}
        }
    }

    /// `InputPageBlock` 的 24 个构造器，一个一个点名：现在**每一个**都有 md / HTML 入口。
    ///
    /// 这张表就是本模块的能力边界。TDLib 加块/删块会让它失败，逼着我们补语法
    /// 或者明确「不支持」——而不是让某个块悄悄只能靠 `with_block` 手拼。
    #[test]
    fn input_page_block_coverage_is_accounted_for() {
        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        let cases: [(&str, &str); 24] = [
            ("inputPageBlockSectionHeading", "# 标题"),
            ("inputPageBlockParagraph", "段落"),
            ("inputPageBlockPreformatted", "```rust\nfn a() {}\n```"),
            ("inputPageBlockFooter", "<footer>页脚</footer>"),
            (
                "inputPageBlockThinking",
                "<tg-thinking>思考中</tg-thinking>",
            ),
            ("inputPageBlockDivider", "---"),
            ("inputPageBlockMathematicalExpression", "$$\nE\n$$"),
            ("inputPageBlockAnchor", "<a name=\"a\"></a>"),
            ("inputPageBlockList", "- 项"),
            ("inputPageBlockBlockQuote", "> 引用"),
            (
                "inputPageBlockExpandableBlockQuote",
                "<blockquote expandable>x</blockquote>",
            ),
            ("inputPageBlockPullQuote", "<aside>x<cite>y</cite></aside>"),
            ("inputPageBlockAnimation", "![](https://x/a.gif)"),
            ("inputPageBlockAudio", "![](https://x/a.mp3)"),
            ("inputPageBlockDocument", "![](https://x/a.zip)"),
            ("inputPageBlockPhoto", "![](https://x/a.jpg)"),
            ("inputPageBlockVideo", "![](https://x/a.mp4)"),
            ("inputPageBlockVoiceNote", "![](https://x/a.ogg)"),
            (
                "inputPageBlockCollage",
                "<tg-collage><img src=\"https://x/a.jpg\"/></tg-collage>",
            ),
            (
                "inputPageBlockSlideshow",
                "<tg-slideshow><img src=\"https://x/a.jpg\"/></tg-slideshow>",
            ),
            ("inputPageBlockTable", "| a |\n|:--|\n| 1 |"),
            (
                "inputPageBlockDetails",
                "<details><summary>s</summary>b</details>",
            ),
            ("inputPageBlockMap", "<tg-map lat=\"1\" long=\"2\"/>"),
            (
                "inputPageBlockButtonRow",
                "<tg-button-row><tg-button type=\"url\" url=\"https://x\">b</tg-button></tg-button-row>",
            ),
        ];
        // 上面第 1、2 项都是 sectionHeading（md 标题与 <h3>），所以去重后是 24 个不同构造器
        let mut names: Vec<&str> = cases.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 24, "InputPageBlock 一共 24 个构造器");

        for (expected, markdown) in cases {
            let blocks = markdown::to_blocks(markdown);
            assert_eq!(blocks[0]["@type"], expected, "输入：{markdown}");
        }
        // HTML 标题与 md 标题走同一条路
        assert_eq!(
            markdown::to_blocks("<h3>html 标题</h3>")[0]["@type"],
            "inputPageBlockSectionHeading"
        );
        for name in names {
            assert!(schema.get(name).is_some(), "真 schema 里没有 {name}");
        }
        // 列表项属于 `InputPageBlockListItem` 类，不算在 24 个里
        assert!(schema.get("inputPageBlockListItem").is_some());
    }

    /// 我们拼的每个构造器都必须真的在编译期烤进来的真 schema 里。
    ///
    /// 这条挡住两类漂移：`td_api.tl` 换版本后删/改了节点，以及我们手拼的 `@type` 打错字
    /// （`check_request` 只看顶层参数名，嵌套的构造器名它管不着）。
    #[test]
    fn every_constructor_we_emit_exists_in_the_real_schema() {
        let schema = fuyubot_tdlib_sys::TdApiSchema::compiled();
        let markdown = "# 标题 **粗** *斜* ~~删~~ `码` ==标记== ||剧透|| $x^2$ <u>下划线</u> <sub>2</sub> <sup>3</sup>\n\n\
<details open><summary>摘要</summary>\n\n正文[^n] <tg-spoiler>剧透</tg-spoiler>\n\n</details>\n\n\
![](https://x/a.jpg \"图注\")\n\n\
| a | b |\n|:--|--:|\n| 1 | 2 |\n\n\
> 引用\n\n<blockquote expandable>折叠</blockquote>\n\n\
<aside>拉引<cite>作者</cite></aside>\n\n<footer>页脚</footer>\n\n\
<a name=\"ch1\"></a>\n\n<tg-map lat=\"1\" long=\"2\" zoom=\"10\"/>\n\n\
<figure><img src=\"https://x/p.jpg\" tg-spoiler/><figcaption>说明<cite>署名</cite></figcaption></figure>\n\n\
<tg-collage><img src=\"https://x/a.jpg\"/><video src=\"https://x/b.mp4\"></video></tg-collage>\n\n\
<tg-slideshow><img src=\"https://x/a.jpg\"/></tg-slideshow>\n\n\
<tg-button-row align=\"left\"><tg-button type=\"url\" url=\"https://t.me\" style=\"primary\">链接</tg-button><tg-button type=\"copy_text\" text=\"x\">复制</tg-button><tg-button type=\"callback_data\" data=\"c\">回调</tg-button><tg-button type=\"web_app\" url=\"https://a\">应用</tg-button><tg-button type=\"url\" url=\"tg://user?id=7\">人</tg-button><tg-button type=\"disabled\">禁用</tg-button></tg-button-row>\n\n\
<tg-thinking>思考中</tg-thinking>\n\n\
<pre><code class=\"language-rust\">fn a() {}</code></pre>\n\n\
```math\nE=mc^2\n```\n\n\
[^n]: 脚注定义。\n\n\
[链](https://x) [邮](mailto:a@b) [话](tel:+1) [人](tg://user?id=1) [跳](#a) \
![😀](tg://emoji?id=1) ![时](tg://time?unix=1&format=wDT) <tg-emoji emoji-id=\"2\">😀</tg-emoji> \
<tg-reference name=\"r\">被引用</tg-reference> <a name=\"x\"></a>";
        let mut emitted = std::collections::BTreeSet::new();
        collect_types(&Value::Array(markdown::to_blocks(markdown)), &mut emitted);

        // 三种源 + 按钮 + md 覆盖不到的 10 个表达式
        let document = json!({
            "@type": "document",
            "file_name": "a.png",
            "mime_type": "image/png",
            "document": { "@type": "file", "id": 1, "size": 1, "expected_size": 1 },
        });
        let extras = vec![
            rich_content_json(
                &RichMessage::server_markdown("x")
                    .with_media(vec![RichMedia::new("m", Media::photo("/tmp/a.jpg"))]),
                true,
            ),
            RichMessage::server_html("<b>x</b>").to_json(),
            rich_content_json(&RichMessage::from_markdown("x"), false),
            richtext::mention(richtext::plain("x"), "u"),
            richtext::hashtag(richtext::plain("x"), "#t"),
            richtext::cashtag(richtext::plain("x"), "$USD"),
            richtext::bank_card_number(richtext::plain("x"), "4242"),
            richtext::ton_address(richtext::plain("x"), "EQ"),
            richtext::bot_command(richtext::plain("x"), "/start"),
            richtext::icon(document, 16, 16),
            richtext::button(&RichButton::url("b", "https://x")),
            richtext::diff(richtext::plain("new"), richtext::plain("old")),
            richtext::anchor("a"),
            RichButton::url("b", "https://x")
                .style(RichButtonStyle::Danger)
                .to_json(),
        ];
        collect_types(&Value::Array(extras), &mut emitted);

        assert!(emitted.len() > 50, "收集到的构造器太少：{}", emitted.len());
        for name in &emitted {
            assert!(schema.get(name).is_some(), "真 schema 里没有 {name}");
        }
    }

    /// 递归收集所有 `@type`（不限 `richText*`）。
    fn collect_types(value: &Value, out: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(kind) = map.get("@type").and_then(Value::as_str) {
                    out.insert(kind.to_owned());
                }
                for child in map.values() {
                    collect_types(child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_types(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn send_and_edit_params_use_tdlib_field_names() {
        let params = RichMessageParams {
            reply_to_message_id: Some(9),
            topic_id: Some(Topic::forum(3)),
            reply_markup: vec![vec![ReplyButton::callback("好", "ok")]],
            ..RichMessageParams::markdown("正文")
        };
        let send = rich_send_params(7, &params);
        // 真签名是 topic_id / reply_to / input_message_content（不是 Bot API 那套）
        assert_eq!(send["chat_id"], 7);
        assert_eq!(send["topic_id"]["@type"], "messageTopicForum");
        assert_eq!(send["topic_id"]["forum_topic_id"], 3);
        assert_eq!(send["reply_to"]["@type"], "inputMessageReplyToMessage");
        assert_eq!(
            send["input_message_content"]["@type"],
            "inputMessageRichMessage"
        );
        assert_eq!(
            send["input_message_content"]["message"]["source"]["@type"],
            "richMessageSourceBlocks"
        );
        assert_eq!(
            send["reply_markup"]["rows"][0][0]["@type"],
            "inlineKeyboardButton"
        );

        let edit = rich_edit_params(7, 9, &RichMessageParams::markdown("改过"));
        assert_eq!(edit["message_id"], 9);
        assert_eq!(
            edit["input_message_content"]["message"]["source"]["blocks"][0]["@type"],
            "inputPageBlockParagraph"
        );
    }
}
