# ts-counter：TypeScript 示例插件

数**每个聊天**收到多少条消息，每收到一条就回一条「本聊天第 N 条消息」。
逻辑全在 [`counter.ts`](counter.ts)（TypeScript），Rust 那 12 行只是 ABI 胶水。

## 它想证明什么

**`fuyubot-sdk codegen --lang js` 落的 `.d.ts` 是能给 TypeScript 用的**，
而且 TS 插件**不需要额外的编译步骤**——Node ≥ 23.6 自带类型剥离，直接跑 `.ts`。

`fuyubot_plugin.js` 与 `fuyubot_plugin.d.ts` 是**生成的绑定**，与
`crates/fuyubot-sdk-script/js/` 里那两份**逐字相同**。
按 `examples/python-weather` 的先例，示例里提交它们、并用一条测试钉住不漂移
（`fuyubot-sdk-cli` 的 `the_example_plugin_bridges_do_not_drift`）。

## 跑起来

```powershell
$env:FUYUBOT_TDLIB_VENDORED_CACHE = "F:\Rust\FuyuBot\.tdlib-cache"   # 构建必需
cargo build -p fuyubot-example-counter --offline                      # → target\debug\counter.dll

# 宿主按「cdylib 同名同目录」找脚本，所以先把 dll 和脚本放一起
$dir = "target\ts-counter-plugins"; New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item target\debug\counter.dll $dir
Copy-Item examples\ts-counter\counter.ts $dir
Copy-Item examples\ts-counter\fuyubot_plugin.js $dir

target\debug\fuyubot.exe run --replay examples\replay\counter.json --plugin-dir $dir
```

回放里三条消息分属两个聊天（A、A、B），预期三条 `sendMessage`：
`本聊天第 1 条消息` / `本聊天第 2 条消息` / `本聊天第 1 条消息`——**B 的计数是独立的**。

也可以单独调试脚本（没有宿主可说话，`host.run()` 读到 EOF 就退出）：

```powershell
node examples\ts-counter\counter.ts
```

## 两条硬约定（脚本插件都适用）

1. **脚本必须与 cdylib 同名同目录**：`counter.dll` ↔ `counter.ts`。
   宿主按动态库文件名去找（cargo 在类 Unix 上加的 `lib` 前缀会被去掉）。
2. **别回自己发的消息**：`message.is_outgoing` 要跳过，否则插件回一条、那条又触发自己，
   就自激了。两个示例都在这里加了判断。

## TypeScript 的边界

用的是 Node 的**类型剥离**，所以语法有限制：注解、`interface` / `type`、`as` 都行，
**`enum` / `namespace` / 构造器参数属性不行**（那些需要真正的编译）。
Node 太老（< 23.6）就自己 `tsc` 编成 `.js`——**编译与打包是插件作者自己的工具链**，
两个 CLI 都不参与（见 `docs/ROADMAP.md` 变更 5）。

## 相关文档

- [`docs/plugin-guide.md`](../../docs/plugin-guide.md)：插件开发总览
- [`docs/abi-contract.md`](../../docs/abi-contract.md)：`HostApiVTable` 的契约
- [`examples/python-weather`](../python-weather)：同样走脚本桥，但逻辑是 Python
