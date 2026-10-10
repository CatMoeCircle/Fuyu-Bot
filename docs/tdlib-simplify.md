# TDLib 便捷封装对照表（`fuyubot-simplify`）

结论先说：**封装只简化输入，不改变输出**。每个方法都经 `HostApi::td_invoke_simplified`
（或 `td_invoke`）发出，不绕过 ABI、不碰 TDLib 的库；原始更新照旧全部对插件可见，
封装只是可选糖。所有「实际发出的方法名」都由 `src/lib.rs` 里的常量表 / `method_for` 决定。

**验证状态**：方法名与字段路径**没有被真库验证过**（HANDOFF 缺口 11）。单元测试拿
`fuyubot-loader::HostRuntime` 当假 TDLib 用（请求留档 + 合成请求号 + 手动投递响应），
所以「拼装对不对」验过，「TDLib 认不认」没验过。

## 1. 公开方法

`Simplify::new(host)` / `Simplify::host()` 只是持有并交出 `HostApi`（`Simplify` 是 `Copy`）。

| 封装方法 | 发出的 TDLib 方法（`@type`） | 关键参数 | 返回类型 | 备注 |
|---|---|---|---|---|
| `send_message(chat_id, content, options)` | `sendMessage` / `sendPhoto` / `sendDocument` / `sendLocation`——由 `method_for(options.account_type, &content)` 决定，`@type` 由宿主补 | `chat_id`；`Text` → `input_message_content`（`@type=inputMessageText`，`text.text`，`text.entities=[]`，非 Plain 时加 `parse_mode`）；`Photo`/`Document` → `photo`/`document`（`@type=inputFileLocal`，`path`）+ 可选 `caption`；`Location` → 顶层 `latitude`/`longitude`。选项：`message_thread_id`、`reply_to_message_id`、`disable_notification`、`protect_content`、`reply_markup` | `Result<RequestId>` | 只给请求号。TDLib 的临时消息 JSON 从 `"response"` 通道回来（只发给发起请求的那个插件） |
| `send_message_wait_success(chat_id, content, options)` | 同上，之后在 `updateMessageSendSucceeded`（过滤 `message.chat_id == chat_id`）上等 | 同上 + `options.timeout_ms` 当等待上限 | `Result<SendConfirmation>` | 返回的不是裸 `RequestId`，也不是裸 `WaitHandle`：`WaitHandle::payload_json` 是宿主线程局部缓冲，只在下一次宿主调用前有效，直接把那个指针交给调用方是个坑。这里拷成 `Value` 再连同 `request_id` 一起返回 |
| `send_message_with_keyboard(chat_id, text, buttons)` | `sendMessage` | 与 `send_message` 相同，另加 `reply_markup` = `{"@type":"replyMarkupInlineKeyboard","rows":[[…]]}` | `Result<RequestId>` | 内部就是 `SendOptions { reply_markup: Some(inline_keyboard(buttons)), ..Default::default() }` |
| `answer_callback_query(callback_query_id, text, show_alert)` | `answerCallbackQuery` | `callback_query_id`；`text`（`None` → `""`）；`show_alert`；`url=""`；`cache_time=0` | `Result<RequestId>` | 点了内联按钮必须回，否则客户端一直转圈 |
| `wait_for_update(filter, timeout_ms)` | 不发 TDLib 方法（纯订阅） | `filter` 由 `UpdateFilter::to_json()` 生成；其中 `event_type` 键**选通道**，其余键做负载字段匹配 | `WaitHandle` | **阻塞**当前线程，直到匹配 / 超时 / 被 `cancel_wait` 取消。⚠️ 返回的 `payload_json` 下次宿主调用前就失效 |
| `wait_for_update_json(filter, timeout_ms)` | 同上 | 同上 | `Result<Option<Value>>` | 超时或被取消返回 `None`。内部 `payload_of` 当场把临时指针拷成 `Value` |
| `cancel_wait(handle)` | 不发 | `WaitHandle` | `()` | 从另一个线程取消一个还没结束的等待 |
| `resolve_entities(ids, timeout_ms)` | 每个 `EntityId` 一个请求：`EntityId::Chat(_)` → `getChat`（参数 `chat_id`）；`EntityId::User(_)` → `getUser`（参数 `user_id`） | 先把所有请求**并发发出去**，再逐个在 `"response"` 通道上按 `request_id` 等 | `Result<EntitiesResolution>` | 返回的不是裸 `RequestId`：每个实体带自己的 `request` 与 `value`（`value` 超时是 `Null`）。顺序是「请求并发发、响应逐个等」，每个的等待上限都是 `timeout_ms`。聚合结果另发一条 `"entities_resolved"` 事件；`EntitiesResolution::request_id` 是**最后一条发出的请求**，只为日志关联 |
| `download_file(file_id, priority, timeout_ms)` | `downloadFile` | `file_id`；`priority`；`offset=0`；`limit=0` | `Result<FileDownload>` | 阻塞到完成或超时；每收到一条 `updateFile` 推一个 `"file_progress"` 事件。过滤器是 `{"event_type":"update","file.id":N}`——**file id 在 `file.id`，不是顶层**（TDLib 的嵌套结构） |

## 2. 过滤器与选择器

| 项 | 产生的 JSON / 写法 | 备注 |
|---|---|---|
| `UpdateFilter::new(type)` | `{"event_type":"<type>","message.chat_id":…}` | 字段包在 `message` 里的更新（`updateNewMessage` 等），前缀是 `message.` |
| `UpdateFilter::flat(type)` | `{"event_type":"<type>","chat_id":…}` | 字段在顶层的更新（`updateNewCallbackQuery` 等），不加前缀 |
| `UpdateFilter::any()` | `{"event_type":"update"}` | 不限定类型，走兜底通道 |
| `UpdateFilter::to_json()` | `chat_id` / `sender_user_id` / `message_thread_id` 三个可选字段 | `event_type` 的值是具体 `@type` 或兜底 `update`；它**不参与负载匹配** |
| `EntityId::to_json()` | `{"kind":"chat|user","id":N}` | 只用于 `EntitiesResolution::to_json` |
| `Button::callback(text, data)` | `{"@type":"inlineKeyboardButton","text":…,"type":{"@type":"inlineKeyboardButtonTypeCallback","data":<base64>}}` | TDLib 的 callback data 是 **base64** 编码的字节串；`data` 由 `base64()` 就地编码 |
| `Button::url(text, url)` | `…"@type":"inlineKeyboardButtonTypeUrl","url":…` | 用户账号不能发 URL 按钮，TDLib 会拒绝 |

## 3. `(账号类型, 内容) → 方法名` 路由表

`AccountType` 保留在签名里，但 **TDLib 1.8.66 里两种账号取值相同**——
`sendMessage` / `sendPhoto` / `sendDocument` / `sendLocation` 对 Bot 与 User 通用。
表在那里是为了将来分化时只改一处；单元测试把「两边一样」显式断言下来了。

| 内容 | `AccountType::Bot` | `AccountType::User` |
|---|---|---|
| `MessageContent::Text` | `sendMessage` | `sendMessage` |
| `MessageContent::Photo` | `sendPhoto` | `sendPhoto` |
| `MessageContent::Document` | `sendDocument` | `sendDocument` |
| `MessageContent::Location` | `sendLocation` | `sendLocation` |

`ParseMode` 的映射（`Text` 内容的 `input_message_content.text.parse_mode`）：

| `ParseMode` | 写进请求的值 |
|---|---|
| `Plain`（默认） | 不写这个字段 |
| `Markdown` | `"Markdown"` |
| `MarkdownV2` | `"MarkdownV2"` |
| `Html` | `"HTML"` |

## 4. 不挂在 `Simplify` 上的公开函数

| 函数 | 签名 | 作用 |
|---|---|---|
| `method_for` | `(AccountType, &MessageContent) -> &'static str` | 上面那张路由表 |
| `send_params` | `(i64, &MessageContent, &SendOptions) -> Value` | 组装参数，**不含 `@type`**（那是宿主补的） |
| `inline_keyboard` | `(Vec<Vec<Button>>) -> Value` | 构造 `replyMarkupInlineKeyboard` |

`resolve_entities` 发 `getChat`/`getUser`、`answer_callback_query` 发
`answerCallbackQuery`、`download_file` 发 `downloadFile`——这三处的 `sendMessage` 同类约定
都成立：返回值是请求号或等待结果，**没有一个是裸 `WaitHandle`**。

## 5. 返回值长什么样

| 类型 | 字段 | 说明 |
|---|---|---|
| `SendConfirmation` | `request_id: RequestId`、`confirmed: bool`、`update: Value` | `confirmed` = 等到了 `updateMessageSendSucceeded`；`update` 是那条更新的原始 JSON，超时是 `Null` |
| `FileDownload` | `request_id: RequestId`、`file_id: i32`、`completed: bool`、`last_update: Value` | `completed` 取自 `file.local.is_downloading_completed` |
| `EntitiesResolution` | `request_id: RequestId`、`entities: Vec<EntityResult>` | `to_json()` 给出 `{"request_id":…,"entities":[…]}` |
| `EntityResult` | `id: EntityId`、`request: RequestId`、`value: Value` | 每个实体自己的请求号与响应；超时 `value` 是 `Null` |
| `SimplifyError` | `Abi(AbiError)`（`#[from]`，`transparent`）、`BadString` | 只有两种：宿主调用失败，或参数串里含 NUL 字节 |

`Simplify` 顺带推的几条简化事件（想看原始更新就直接订阅原始 `@type`，两者不冲突）：

| 事件 | 谁发 | 负载 |
|---|---|---|
| `entities_resolved` | `resolve_entities` 末尾 | `EntitiesResolution::to_json()` |
| `file_progress` | `download_file` 每收到一条 `updateFile` | `{"file_id":…,"completed":…,"downloaded_size":…,"expected_size":…}` |

## 6. 用法（Rust 插件侧）

`fuyubot-simplify` 只依赖 `fuyubot-abi`，所以插件 crate 可以直接依赖它：

```rust
use fuyubot_simplify::{Simplify, MessageContent, SendOptions};

let simplify = Simplify::new(self.host);          // HostApi 是 Copy
let request = simplify.send_message(
    chat_id,
    MessageContent::text("你好"),
    &SendOptions::default(),
)?;
self.host.info(&format!("发出去了：{}", request));
```

`MessageContent` / `SendOptions` / `Button` / `UpdateFilter` / `EntityId` 都是从
`fuyubot_simplify` 根导出的。

## 7. 与请求号有关的两条时间线

1. `td_invoke*` 返回的是**请求号**，真正的 JSON 从 `"response"` 通道回来
   （事件类型 `RESPONSE_EVENT`，只发给发起请求的那个插件）。
2. `WaitHandle::payload_json` 是宿主临时的**线程局部缓冲**，只在下一次宿主调用之前有效。
   要拿数据就立刻读，或者用 `*_json` 系列（它们已经替你拷好了）。

`HostRuntime::td_send` 在**没接 TDLib** 时只留档 + 发合成请求号（从 `1 << 48` 起），
所以本文的每个方法在离线状态下都能被跑通、但不会真的到达 TDLib。
