# fuyubot-simplify

TDLib 便捷封装：把「发消息 / 相册 / 富消息 / 编辑消息 / 删除消息 / 等更新 / 内联键盘 / 批量解析 / 下载进度」包成人话 API——**一个方法一个参数结构体 + 短字段名**（`SendMessageParams { text, media, reply_to_message_id, topic_id, reply_markup, link_preview, timeout_ms }`），调用方不拼 TDLib 的嵌套 JSON，请求经 `td_invoke*` 发出、返回的仍是 TDLib 原始 JSON。

`formattedText` 那条链路上的带标记文本（Markdown / HTML）**不自己解析**——经同步的 `td_execute` 直接问 TDLib 的 `parseTextEntities`。

富消息（`messageRichMessage`）是另一套：TDLib 里没有 `sendRichMessage` / `editRichMessage`，也没有本地的 md → 块树转换（md/html 源由**服务端**解析且**仅 bot** 可用）。所以：

- 发送 / 编辑 = `send_rich_message` / `edit_rich_message`（底层就是 `sendMessage` / `editMessageText` + `inputMessageRichMessage`）；
- 三种源都在 `RichMessage` 上：`from_markdown`（**本地**转换，任何账号可发）、`server_markdown` / `server_html`（服务端解析，仅 bot）、`from_blocks`（直接给块树）；
- 本地转换器**对齐 Bot API 的 Rich Markdown 全表**（<https://core.telegram.org/bots/api/rich-messages>）：标题 / 段落 / 围栏代码（含 `math`）/ 引用与可展开引用 / 拉引 / 页脚 / 列表与任务项 / 表格 / 分割线 / 公式 / 折叠块 / 拼贴与幻灯片 / 媒体（按扩展名猜类型）/ 地图 / 锚点 / 按钮行 / 思考占位 / 脚注，行内粗斜删码、标记、剧透、上下标、行内公式、自定义 emoji、时间、mailto/tel/tg://user/锚点链接，以及文档里允许的那批 HTML 标签（`<u>` `<details>` `<tg-collage>` `<tg-button-row>` …）。不认的（引用式链接、`<table>` 这类 HTML 块）一律当普通文本；
- 按钮在 Markdown 里**没有语法**，所以是显式的：`RichButton::url / callback / user / copy_text` + `with_buttons` / `with_button_row` 追加 `inputPageBlockButtonRow`（只对 Blocks 源成立，md/html 源会报 `RichSourceNotBlocks`）；
- `InputPageBlock` 一共 24 个构造器，本地转换器**全都能产出**（`thinking` 只能出现在草稿消息里，结构上仍可构造）；`with_block` 留给「媒体已经上传、要传 `inputPhoto` 句柄」这类 md 表达不了的场景。分工表在 `input_page_block_coverage_is_accounted_for` 测试里，加块/删块会让它失败；
- 块级语法**按官方 Rich Markdown 表一类一例**在 `block_syntax_covers_the_official_table` 里断言（每一块都会再走一遍 `td_api::InputPageBlock` 反序列化），整段全格式的回归闸是内联的 `official_sample_fully_deserializes`；
- 行内表达式是独立的 `RichText`（31 个构造器、没有 offset/length）：Markdown 覆盖 21 个，剩下 10 个（`mention` / `hashtag` / `cashtag` / `bankCardNumber` / `tonAddress` / `botCommand` / `icon` / `button` / `diff` / `anchor`）没有 md 语法，用 `richtext::*` 拼好塞进块。31 个全部有反序列化测试。
