# fuyubot-loader

宿主层：`HostRuntime` 把 `HostApiVTable` 的每个函数桥到四大机制与 TDLib，`PluginLoader` 用 libloading 加载 cdylib 插件并做 ABI 版本协商。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`HostRuntime`** —— `HostApiVTable` 的唯一实现：事件、服务、工具、中间件、`TdClient` 全从这里桥出去；每个 dylib 插件有自己的 `Context` 子作用域，`rollback_owner` 连作用域一起丢。
- **命令能力表** —— 插件经 `fuyubot.host.commands` 登记命令（角色 / 会话范围 / 是否隐藏），`/help` 与命令闸门读同一张表；归属由调用者栈判定，卸载按 owner / scope 回滚。
- **TDLib 对象缓存** —— 全量缓存 chat / user / basic_group / supergroup / chat_member / community / community_full_info，每种默认 100 条、LRU；`update*` 在中间件之前刷新，`chat_member` 带 30 秒 TTL，离线未命中直接返回。chat list 是独立索引，条目不占 chat 配额，chat 被淘汰后列表项仍在；`list_groups` 把 main / archive / chat folder 分好组，folder 标题来自 `updateChatFolders`。
- **`PluginLoader` / `DylibPlugin`** —— 加载 cdylib、读 `plugin_manifest`、判定 ABI 版本（`fuyubot_abi_version`），不匹配即拒；依赖声明以 manifest 为准。
- **`UpdatePipeline`** —— 更新走六阶段 + 双通道（具体 `@type` 与 `"update"`），响应按 `@request_id` 回到发起插件。
- **`script`** —— 脚本插件宿主侧：spawn 中间插件、stdio 钉 UTF-8、订阅号路由。

所有动态库加载走 `loader::open_library`（先查库自己所在目录）；注册失败按 owner 整体回滚。

## 相关

- [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
