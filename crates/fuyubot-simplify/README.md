# fuyubot-simplify

TDLib 便捷封装：把「发消息 / 相册 / 编辑消息 / 删除消息 / 等更新 / 内联键盘 / 批量解析 / 下载进度」包成人话 API——**一个方法一个参数结构体 + 短字段名**（`SendMessageParams { text, media, reply_to_message_id, topic_id, reply_markup, link_preview, timeout_ms }`），调用方不拼 TDLib 的嵌套 JSON，请求经 `td_invoke*` 发出、返回的仍是 TDLib 原始 JSON。
带标记的文本（Markdown / HTML）**不自己解析**——经同步的 `td_execute` 直接问 TDLib 的 `parseTextEntities`。
