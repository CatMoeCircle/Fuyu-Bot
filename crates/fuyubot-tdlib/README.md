# fuyubot-tdlib

TDLib 接入层：`ffi` 声明 C 函数指针，`loader` 在运行时按优先级找到并加载原生库，`client` 提供 `TdClient`（`@extra` 注入 + `td_receive` 循环）。

`td_api.tl` 是**构建的必需输入**（拿不到就让构建失败），在编译期被转成两份产物：`schema`（可校验的元数据）与 `td_api`（Rust 类型，`tl_codegen` 生成）。运行时不读文件、不生成。
