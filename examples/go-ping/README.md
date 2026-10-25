# go-ping —— Go 写的 ping 插件

收到 `/ping` 回 `pong`。和 [`../c-ping`](../c-ping) 干同一件事，区别只是逻辑写在 Go 里。

结构分两半：

| 文件 | 作用 |
| --- | --- |
| `main.go` | 真正的逻辑：`encoding/json` 解析更新、拼 `sendMessage`、打日志 |
| `plugin.c` | 四个 ABI 导出符号的薄壳：把宿主的回调转成对 Go 函数的调用 |
| `fuyubot_plugin.h` | **生成物，不要提交**：`fuyubot-sdk codegen --lang c --dir .` 落的 |

为什么不直接 `//export plugin_init`：`PluginHandle` 是**按值返回**的结构体，
由 C 侧构造最省事；也避免把宿主 VTable 的函数指针搬进 Go 的代码。

## 编译

Go 的 `c-shared` 需要 **cgo**，也就是一台 C 编译器：Linux / macOS 用系统自带的 `cc`，
Windows 用 mingw-w64（例如 [w64devkit](https://github.com/skeeto/w64devkit)）。

```powershell
# 1. 落 ABI 头（cbindgen 生成的唯一权威副本，不许手改）
fuyubot-sdk codegen --lang c --dir examples\go-ping

# 2. 编成动态库（名字决定插件文件名，宿主按同名找脚本——Go 插件没有脚本）
$env:CGO_ENABLED = "1"
$env:CC = "gcc"                                  # mingw-w64 的 gcc；Linux/macOS 用 cc 或 clang
go build -buildmode=c-shared -o ping.dll .
```

Linux / macOS 把输出名换成 `ping.so` / `ping.dylib`（或者用 `.dll` 也行，
`fuyubot plugin install` 按扩展名识别）。

> 本机（Windows + MSVC 工具链）没有 mingw 时 `go build` 会报
> `gcc: executable file not found` 或 `cannot open file 'mingwex.lib'`——那是**缺 C 工具链**，
> 和插件代码无关。把一个 mingw 的 `bin` 加进 `PATH`、`CC` 指到它的 `gcc` 即可。

## 验证

```powershell
fuyubot-sdk check ping.dll                       # 静态读导出符号，不加载
fuyubot run --replay ping.json --plugin-dir <放 ping.dll 的目录>
```

回放文件（`updateNewMessage` 里正文是 `/ping`）：

```json
{"@type":"updateNewMessage","message":{"@type":"message","id":1,"chat_id":-1001234567890,
 "is_outgoing":false,"content":{"@type":"messageText",
 "text":{"@type":"formattedText","text":"/ping","entities":[]}}}}
```

期望输出里有：

```text
[plugin/Info] go-ping 插件：start()
[plugin/Info] go-ping 插件：回了 pong
  #281474976710656 sendMessage {"@type":"sendMessage","chat_id":-1001234567890,
    "input_message_content":{"@type":"inputMessageText","text":{...}}}
[plugin/Info] go-ping 插件：stop()
```

## 一个必须知道的宿主行为

**Go 的 `c-shared` 库不能卸载**：它的运行时有线程和信号处理器，`FreeLibrary` / `dlclose`
之后进程会直接 access violation（0xC0000005）。

所以 `fuyubot-loader` 会识别 Go 产物（文件里的 `\xff Go buildinf:` 魔数），
对这类库**故意不卸载**——`plugin_stop` 照跑、注册照回滚，只是不释放那份库内存，
让它一直映射到进程退出。普通 Rust / C 插件不受影响，还是 drop 即卸载。
