# fuyubot-android

Android 的 JNI 运行时：把整个宿主（内核 + 宿主运行时 + TDLib 接入 + 插件 + 登录）编成
`libfuyubotjni.so`，APK 作者 `System.loadLibrary("fuyubotjni")` 就能用。

**只交付 `.so`**：不出 APK、不出 UI、不出 Java 代码。保活（前台 Service / 通知 / WakeLock）
是 APK 作者的事，本 crate 只提供幂等的 `start` / 必须被调的 `stop` / 如实的 `onState`。

JNI 绑定是**运行期**的：`JNI_OnLoad` 用 `RegisterNatives` 绑到
`com.catmoecircle.fuyubot.runtime.Native`（包名常量 `NATIVE_CLASS`；构建时给
`FUYUBOT_JNI_CLASS=com/你的/包/Native` 覆盖，必须是 `/` 分隔的内部名）。导出符号里
没有 `Java_...`，换包名不用改库。因为绑定按名字做，**开了 R8 / ProGuard 要 keep 这个类**：
`-keep class com.catmoecircle.fuyubot.runtime.Native { *; }`。

ABI **v1** 一共十二个方法：六个生命周期/问答，加上 `nativePlugins()`（插件列表 JSON）、
`nativePluginInfo(name)`（单个插件）、`nativeAuthState()`（TDLib 授权状态 JSON）、
`nativeConnectionState()`（TDLib 连接状态 JSON）、`nativeTdRequest(json, timeoutMs)`
（原始 td_api JSON 进、响应 JSON 出，阻塞）、`nativeLastError()`（本线程最后一次错误）。
对应的 C 符号是 `fuyubot_plugins` / `fuyubot_plugin_info` / `fuyubot_auth_state` /
`fuyubot_connection_state` / `fuyubot_td_request`。

**两个 TDLib 状态都是「订阅后记最新一条」**（TDLib 没有可查询的方法）：

- 授权状态：`{"known":true,"state":"authorizationStateReady","ready":true,"needs_re_login":false}`。
  登录窗口里的中间态（`authorizationStateWaitCode` 等）也查得到——`log_in_with` 把
  `updateAuthorizationState` 自己消费掉了，本库在 `Prompter::auth_state` 钩子里记下来。
- 连接状态：`{"known":true,"state":"connectionStateReady","ready":true}`，
  五种状态（等网络 / 连代理 / 连接中 / 同步中 / 就绪）。订阅在登录**之前**装好，
  所以登录窗口里连不上也如实反映。

运行时没起来 / 一条更新都还没收到时，两个接口返回同形状的 `known:false`、`state:null`
（合法 JSON，不是 `null`、也不是错误）。两个查询都只做一次 JSON 序列化，任意线程可调。

**插件加载结果会主动上报**：装完插件、**登录之前**调一次回调
`Callback.onPlugins(String)`，参数与 `nativePlugins()` 同一份 JSON——
`count` / `scanned` / `active` / `pending` / `activating` / `failed` / `unknown` /
`skipped` 八个计数，`plugins` 清单（失败的带 `reason`），以及连内核都没进的
`skipped_plugins`（探测失败 / 位置不对的脚本，带原因）。`onPlugins` 是可选的；
`nativePlugins()` 从插件装好那一刻起就能查，登录窗口里也拿得到。

**运行期调日志不用改 ABI**：往 `<root>/.fuyubot/log-level` 写一行级别名
（`error` / `warn` / `info` / `debug` / `trace`），库内每 500ms 看一次、变了立刻生效；
文件不存在 = 不改，写错 = 保持当前级别并留一条 warn。级别只管插件日志的门槛，
删文件不会回退（写回 `info` 或下次 `start` 用 `options.log_level`）。
日志本身在 `<root>/logs/fuyubot.log`。

对外头文件是 [`include/fuyubot_jni.h`](include/fuyubot_jni.h)，它同时也是宿主契约。
