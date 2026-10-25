# TDLib 便捷封装对照表（`fuyubot-simplify`）

结论先说：**封装只简化输入，不改变输出**。参数形状是「一个方法一个参数结构体 + 短字段名」
（`text` / `media` / `topic_id` / `reply_to_message_id` / `reply_markup` / `link_preview` /
`timeout_ms`），**不让调用方拼 TDLib 的嵌套 JSON**；请求本身照旧经 `td_invoke*` 发出，
文本格式经同步的 `td_execute` 问 TDLib，原始更新对插件完全可见，封装只是可选糖。

**验证状态**：方法名与参数名由 `every_request_simplify_builds_passes_the_real_schema`
拿真 `td_api.tl` 烤出来的元数据（`TdApiSchema::compiled()`）逐条校验，离线就能跑；
Markdown → entities 的**真实行为**由
`crates/fuyubot-tdlib/tests/integration.rs` 的 `execute_runs_synchronous_methods`
（`-- --ignored`，要真库）钉住。单元测试拿 `fuyubot-loader::HostRuntime` 当假 TDLib 用。

## 1. 公开方法

`Simplify::new(host)` / `Simplify::host()` 只是持有并交出 `HostApi`（`Simplify` 是 `Copy`）。

| 方法 | 发出的 `@type` | 参数结构体 | 返回 |
|---|---|---|---|
| `send_message(chat_id, &SendMessageParams)` | `sendMessage` | `text` / `media` / `reply_to_message_id` / `topic_id` / `reply_markup` / `link_preview` / `timeout_ms` | `RequestId`（不阻塞） |
| `send_message_wait_success(chat_id, &SendMessageParams)` | 同上，之后等 `updateMessageSendSucceeded` | 同上（`timeout_ms` 是等待上限） | `SendConfirmation`（⚠️ **阻塞**） |
| `send_message_album(chat_id, &SendAlbumParams)` | `sendMessageAlbum` | `medias`（2–10 项）/ `caption` / `reply_to_message_id` / `topic_id` | `RequestId`；项数不对返回 `SimplifyError::AlbumSize`。⚠️ **相册没有 `reply_markup`**（真签名里就没有这个参数，schema 守卫抓出来的）：想给相册挂键盘用 `editMessageReplyMarkup` |
| `edit_message_text(chat_id, message_id, &EditTextParams)` | `editMessageText` | `text` / `link_preview` / `reply_markup` | `RequestId` |
| `edit_message_caption(chat_id, message_id, &EditCaptionParams)` | `editMessageCaption` | `text` / `reply_markup` | `RequestId` |
| `edit_message_media(chat_id, message_id, &EditMediaParams)` | `editMessageMedia` | `text`（新内容标题）/ `media` / `reply_markup` | `RequestId` |
| `delete_message(chat_id, ids, revoke)` | `deleteMessages` | `message_ids: [i53]` / `revoke` | `RequestId` |
| `answer_callback_query(query_id, text, show_alert)` | `answerCallbackQuery` | `text`（`None` → `""`）/ `show_alert` / `url=""` / `cache_time=0` | `RequestId` |
| `formatted(&Text)` | **`parseTextEntities`（同步，`td_execute`）** | `text` / `parse_mode` | `formattedText` 的 `Value` |
| `wait_for_update` / `wait_for_update_json` | 不发方法（纯订阅） | `UpdateFilter` + `timeout_ms` | `WaitHandle` / `Option<Value>`（⚠️ 阻塞） |
| `wait_response(request, timeout_ms)` | 不发方法（等 `"response"` 通道） | `RequestId` | `Option<Value>`（⚠️ 阻塞） |
| `resolve_entities(ids, timeout_ms)` | `getChat` / `getUser` | `Vec<EntityId>` | `EntitiesResolution`（⚠️ 阻塞） |
| `download_file(file_id, priority, timeout_ms)` | `downloadFile` | — | `FileDownload`（⚠️ 阻塞） |
| `cancel_wait(handle)` | 不发 | `WaitHandle` | `()` |

**编辑的结果去哪了**：编辑后的 `Message`、以及 `Message is not modified` 这类错误都是**响应**
（`"response"` 通道，`@type == "error"` 表示失败）。要拿就 `wait_response(request, timeout)`。
封装层**刻意不重试**——限流等多久是调用方的事。

**`send_message` 不阻塞、`send_message_wait_success` 阻塞**（对方的实现是一个方法里等完拿
`message`）。分两个入口是因为：等更新要靠**投递线程**送回来，而插件通常正是在更新回调里发消息，
在回调里等就是自己卡自己，等到的是超时。

## 2. 内容与按钮

`Media`（对应 TDLib 的 `inputMessage*`，形状只有一份实现 `content_json`）：

| 变体 | 内容类型 | 必需/常用字段 |
|---|---|---|
| `Media::photo(file)` | `inputMessagePhoto` | `caption` / `width` / `height` / `has_spoiler` |
| `Media::Video { .. }` | `inputMessageVideo` | `duration` / `width` / `height` / `supports_streaming` / `has_spoiler` |
| `Media::Audio { .. }` | `inputMessageAudio` | `duration` / `title` / `performer` |
| `Media::document(file)` | `inputMessageDocument` | `caption` |
| `Media::Animation { .. }` | `inputMessageAnimation` | `duration` / `width` / `height` / `has_spoiler` |
| `Media::Sticker { .. }` | `inputMessageSticker` | `emoji`（**没有 caption**）/ `width` / `height` |
| `Media::location(lat, lng)` | `inputMessageLocation` | — |

- `file` 一律是 `InputFile`：`InputFile::local(path)` → `inputFileLocal`，
  `InputFile::remote(id)` → `inputFileRemote`。`&str` / `String` 会被当成**本地路径**。
- `.caption(text)` 给媒体挂标题；`SendMessageParams.text` 优先于媒体自带的 caption。
- 尺寸/时长填 `0` 也行（TDLib 自己读真实值）。

`ReplyButton`（`reply_markup` 是 `Vec<Vec<ReplyButton>>`，空 = 不带键盘）：

| 构造 | 按钮类型 |
|---|---|
| `ReplyButton::callback(text, data)` | `inlineKeyboardButtonTypeCallback`（`data` 自动 base64） |
| `ReplyButton::url(text, url)` | `inlineKeyboardButtonTypeUrl` |
| `ReplyButton::user(text, user_id)` | `inlineKeyboardButtonTypeUser` |
| `ReplyButton::web_app(text, url)` | `inlineKeyboardButtonTypeWebApp` |

链式修饰：`.emoji(custom_emoji_id)`（`icon_custom_emoji_id`）、
`.style(ButtonStyle::{Default,Primary,Danger,Success})`。

## 3. 文本格式（Markdown / HTML）→ entities

**没有第二套解析器**。带标记的文本经 TDLib 的 `parseTextEntities` 换成
`formattedText { text, entities }`——语法以 TDLib 为准，我们只负责把请求送进去。

| `ParseMode` | 发出的 `parse_mode` |
|---|---|
| `Plain`（默认） | **不发请求**（没有对应的 `TextParseMode`），直接 `formattedText { text, entities: [] }` |
| `Markdown` | `{"@type":"textParseModeMarkdown","version":1}` |
| `MarkdownV2` | `{"@type":"textParseModeMarkdown","version":2}` |
| `Html` | `{"@type":"textParseModeHTML"}` |

快捷方式：`Text::plain/markdown/markdown_v2/html`，`&str` / `String` → 纯文本。
`SendMessageParams::text("hi")` / `SendMessageParams::media(Media::photo("/tmp/a.jpg"))`。

⚠️ **真库实测（TDLib 1.8.68，1.8.66 同结论）：MarkdownV2 的粗体是 `*x*`，不是 `**x**`。**
`**bold**` 会被吞成 `bold` 并且**不给** bold entity，`__x__` 出来是下划线。
别按 CommonMark 的直觉写——这正是「不自己实现解析」的理由。

**为什么走同步的 `td_execute` 而不是「发请求 + 等响应」**：`parseTextEntities` 不碰服务器，
TDLib 允许同步跑它（`td_json_client_execute`）。等响应那套会把**投递线程**卡住
（插件通常正是在更新回调里发消息，响应又要靠同一个线程投递），同步调用没有这个问题。

拿不到结果（没接 TDLib、TDLib 报错）就退回纯文本并打一条 `Warn` 日志：
一条 Markdown 写错的消息不该整条发不出去。

## 4. 参数名为什么是这几个

真 `td_api.tl`：`sendMessage chat_id topic_id reply_to options reply_markup input_message_content`
——**没有** `message_thread_id` / `reply_to_message_id` / 顶层 `disable_notification`
（那三个是 Bot API 的名字），也**没有**顶层 `caption`（它在内容类型里）。
短名字到 TDLib 字段的映射只有一处实现（`send_params` / `put_common`）：

| 参数结构体的字段 | 落到的 TDLib 字段 |
|---|---|
| `topic_id: i64` | `topic_id` = `{"@type":"messageTopicThread","message_thread_id":…}` |
| `reply_to_message_id: i64` | `reply_to` = `{"@type":"inputMessageReplyToMessage","message_id":…}` |
| `reply_markup: Vec<Vec<ReplyButton>>` | `reply_markup` = `{"@type":"replyMarkupInlineKeyboard","rows":…}` |
| `link_preview: Some(false)` | `inputMessageText.link_preview_options.is_disabled = true`（`None`/`Some(true)` 不写这个字段） |
| `text` / `media.caption` | `formattedText`（经 `parseTextEntities`） |

## 5. 不挂在 `Simplify` 上的公开函数

| 函数 | 作用 |
|---|---|
| `send_params` / `album_params` / `edit_text_params` / `edit_caption_params` / `edit_media_params` | 组装参数，**不含 `@type`**（那是宿主补的）；最后一个参数是文本渲染器 |
| `content_json(&Media, render)` | 媒体 → `InputMessageContent` |
| `parse_text_request(&Text)` | `parseTextEntities` 的请求；`Plain` 返回 `None` |
| `formatted_from(&Text, Option<&Value>)` / `is_formatted_text(...)` | 响应 → `formattedText`；拿不到退回纯文本 |
| `plain_text(&Text)` | 纯文本 → `formattedText`（不花往返），也是默认渲染器 |
| `command_of(&Value)` → `Command` | 从 `updateNewMessage` 解出 `chat_id` / `message_id` / `name` / `args` / `text`；跳过自己发的、非文本消息 |
| `on_command(host, 闭包)` → `CommandSubscription` | 订阅 `updateNewMessage`，每条命令调一次闭包；**drop 即退订** |

## 6. 返回值长什么样

| 类型 | 字段 |
|---|---|
| `SendConfirmation` | `request_id`、`confirmed: bool`、`update: Value`（`update.message` 就是最终消息；超时是 `Null`） |
| `FileDownload` | `request_id`、`file_id`、`completed`、`last_update`（`completed` 取自 `file.local.is_downloading_completed`） |
| `EntitiesResolution` | `request_id`、`entities: Vec<EntityResult>`；`to_json()` 给出 `{"request_id":…,"entities":[…]}` |
| `EntityResult` | `id`、`request: RequestId`、`value: Value`（超时是 `Null`） |
| `SimplifyError` | `Abi(AbiError)`、`BadString`（参数串含 NUL）、`AlbumSize(usize)`（相册不是 2–10 项） |

`Simplify` 顺带推的简化事件（想看原始更新就直接订阅原始 `@type`，两者不冲突）：

| 事件 | 谁发 | 负载 |
|---|---|---|
| `entities_resolved` | `resolve_entities` 末尾 | `EntitiesResolution::to_json()` |
| `file_progress` | `download_file` 每收到一条 `updateFile` | `{"file_id":…,"completed":…,"downloaded_size":…,"expected_size":…}` |

## 7. 用法（Rust 插件侧）

`fuyubot-simplify` 只依赖 `fuyubot-abi`（+ 生成类型所在的 `fuyubot-tdlib-sys`），插件 crate 可以直接依赖它：

```rust
use fuyubot_simplify::{
    EditTextParams, Media, ReplyButton, SendMessageParams, Simplify, Text,
};

let simplify = Simplify::new(self.host);          // HostApi 是 Copy

// 纯文本：不花任何往返
simplify.send_message(chat_id, &SendMessageParams::text("你好"))?;

// Markdown + 回复 + 内联键盘
simplify.send_message(
    chat_id,
    &SendMessageParams {
        text: Some(Text::markdown("*粗体*")),
        reply_to_message_id: Some(message_id),
        reply_markup: vec![vec![ReplyButton::callback("确认", "ok")]],
        ..SendMessageParams::default()
    },
)?;

// 图片 + 标题
let photo = Media::photo("/tmp/a.jpg").caption(Text::plain("标题"));
simplify.send_message(chat_id, &SendMessageParams::media(photo))?;

// 编辑：结果从响应通道回来
let request = simplify.edit_message_text(
    chat_id,
    message_id,
    &EditTextParams { text: Some(Text::markdown_v2("*改了*")), ..Default::default() },
)?;
let edited = simplify.wait_response(request, 5_000);

// 删除（对所有人撤回）
simplify.delete_message(chat_id, [message_id], true)?;
```

## 8. 与请求号有关的两条时间线

1. `td_invoke*` 返回的是**请求号**，真正的 JSON 从 `"response"` 通道回来
   （事件类型 `RESPONSE_EVENT`，只发给发起请求的那个插件）。
2. `WaitHandle::payload_json` / `td_execute` 的返回串都是宿主临时的**线程局部缓冲**，
   只在下一次宿主调用之前有效。要拿数据就立刻读，或者用 `*_json` / `wait_response`
   （它们已经替你拷好了）。

`HostRuntime::td_send` 在**没接 TDLib** 时只留档 + 发合成请求号（从 `1 << 48` 起），
`td_execute` 在同样情况下返回 `None`——所以本文的每个方法在离线状态下都能被跑通、
但不会真的到达 TDLib（Markdown 会退回纯文本）。
