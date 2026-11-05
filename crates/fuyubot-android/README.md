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

ABI **v1** 一共十个方法：六个生命周期/问答，加上 `nativePlugins()`（插件列表 JSON）、
`nativePluginInfo(name)`（单个插件）、`nativeTdRequest(json, timeoutMs)`（原始 td_api
JSON 进、响应 JSON 出，阻塞）、`nativeLastError()`（本线程最后一次错误）。
对应的 C 符号是 `fuyubot_plugins` / `fuyubot_plugin_info` / `fuyubot_td_request`。

**插件加载结果会主动上报**：装完插件、**登录之前**调一次回调
`Callback.onPlugins(String)`，参数与 `nativePlugins()` 同一份 JSON——
`count` / `scanned` / `active` / `pending` / `activating` / `failed` / `unknown` /
`skipped` 八个计数，`plugins` 清单（失败的带 `reason`），以及连内核都没进的
`skipped_plugins`（探测失败 / 位置不对的脚本，带原因）。`onPlugins` 是可选的；
`nativePlugins()` 从插件装好那一刻起就能查，登录窗口里也拿得到。

对外头文件是 [`include/fuyubot_jni.h`](include/fuyubot_jni.h)，它同时也是宿主契约。
