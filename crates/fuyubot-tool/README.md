# fuyubot-tool

工具注册表：`ToolDescriptor` 用 JSON Schema 描述入参出参，`call_tool` 依次做权限与 schema 校验再调 C 处理器。

[![](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

## 里面有什么

- **`ToolRegistry`** —— 注册 / 列出 / 查询工具（`list_tools` / `query_tools` 带 JSON 过滤），`revision` 在每次变更后自增，`on_change` 可订阅变更。
- **`ToolDescriptor`** —— 名字、描述、JSON Schema 入参出参；`to_json` / `from_json` 与插件侧交换。
- **`call_tool(name, input)`** —— 先查权限（`grant` / `revoke`），再按 schema 校验入参，最后才调注册时给的处理器；失败走 `ToolError`。
- **`install(&ctx)`** 返回 `Effect`（必须持有），扩展 trait 是 `ToolContextExt`。

JSON Schema 只用本 crate——其他 crate 不得自己拼 schema。

## 相关

- [架构与数据流](https://catmoecircle.github.io/FuyuBot-docs/docs/dev/architecture/)
- [仓库](https://github.com/CatMoeCircle/Fuyu-Bot)
