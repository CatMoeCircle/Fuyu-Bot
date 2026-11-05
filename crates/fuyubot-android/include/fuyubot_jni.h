/*
 * fuyubot_jni.h —— libfuyubotjni.so 的 C 接口（JNI 之外的「逃生门」）。
 *
 * 正常用法是 JNI：APK 里 `System.loadLibrary("fuyubotjni")`，然后调
 * `com.catmoecircle.fuyubot.runtime.Native` 上的 native 方法（清单见下）。本头文件给的是
 * **同一份实现**导出的 C 符号，不想用 Java 就自己写 20 行 JNI（`RegisterNatives`
 * 或 `dlsym`）接上来。
 *
 * 两条铁律：
 *   1. 这些函数都**立即返回**（start 只负责起线程）；真正跑起来 / 失败看 `last_error()` 与日志。
 *      唯一的例外是 `fuyubot_td_request`：它按调用方给的预算等 TDLib 的响应。
 *   2. `last_error()` 与三个返回 JSON 的函数返回的都是**线程局部**缓冲，下次在**同一线程**
 *      调用本库前有效，不能存、不能跨线程传、不能 free。
 *
 * 线程：`start` / `stop` / `is_running` / `answer` / `plugins` / `plugin_info` /
 * `td_request` / `last_error` 可从任意线程调用；`stop` 最多等 8 秒（内部走优雅收摊），
 * 超时返回 FUYUBOT_TIMEOUT。
 * `td_request` **阻塞**到响应回来或超时——从专门的工作线程调，别在 UI 线程上调。
 *
 * options JSON（`start` 的参数，UTF-8、NUL 结尾）：
 *
 *   {
 *     "root":       "/data/user/0/com.example.bot/files",   // 必填、绝对路径、app 私有目录
 *     "plugin_dir": "plugins",                              // 可选，相对 root
 *     "data_dir":   "data",                                 // 可选，相对 root（TDLib 数据库）
 *     "log_level":  "info",                                 // 可选：error/warn/info/debug/trace
 *     "api_id":     12345,                                  // 必填，int32
 *     "api_hash":   "…",                                    // 必填
 *     "bot_token":  "123456:ABC…",                          // 可选；有值就走 bot 登录，不问任何问题
 *     "lib_dir":    "/data/app/…/lib/arm64",                // 可选；缺省时要求宿主先 loadLibrary("tdjson")
 *     "tdlib_log_verbosity": 1                              // 可选；TDLib 自己的日志详细度（0 起）
 *   }
 *
 * 日志写到 <root>/logs/fuyubot.log（会自动追加、自动建目录）。
 */
#ifndef FUYUBOT_JNI_H
#define FUYUBOT_JNI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 运行时 ABI 版本：**运行时判定**，别在编译期假设。
 *
 * 1 → 六个生命周期/问答方法 + 插件列表、插件详情、TDLib JSON 请求、本线程最后一次错误，
 *     共十个 native 方法。
 */
#define FUYUBOT_JNI_ABI 1

/* 返回码。 */
#define FUYUBOT_OK 0        /* 成功 / 已经达成 */
#define FUYUBOT_ARG 1       /* 参数错：JSON 非法、缺字段、root 不是绝对路径… */
#define FUYUBOT_BUSY 2      /* 已经在跑（start 幂等） */
#define FUYUBOT_TIMEOUT 3   /* stop 等收摊超时 */
#define FUYUBOT_PANIC 4     /* 本库内部 panic（已兜住，进程还在） */
#define FUYUBOT_INTERNAL 5  /* 内部错：TDLib 未就绪、登录失败…，细节看 last_error() */

int32_t fuyubot_abi_version(void);

/* 起运行时；立即返回。成功 = FUYUBOT_OK（不代表已登录，看日志 / onState）。 */
int32_t fuyubot_start(const char *options_json);

/* 优雅停车：停接收循环 → 卸插件 → 关 TDLib 会话（刷盘）。上限 8 秒。
 * 超时返回 FUYUBOT_TIMEOUT，并**把卡住的那一步写进 last_error()**：
 *   "stop 等收摊超时（8s）：worker 线程还在跑（kernel 停在这一步：TDLib 关会话（等数据库刷盘））……"
 * 同样的阶段名也会经 onState(STOPPING, …) 逐段推给 Java，App 可以边收边显示。 */
int32_t fuyubot_stop(void);

/* 线程起来且还没收摊 = 1。 */
int32_t fuyubot_is_running(void);

/* 回答一次提问（登录验证码 / 两步密码 / 手机号）。id 来自 Java 回调 onAsk。
 * text 传 NULL = 取消。没有这个 id 在等 = FUYUBOT_ARG。 */
int32_t fuyubot_answer(int32_t id, const char *text);

/* 最后一次错误的文字说明；没有错就是空串。见文件头的线程局部约定。 */
const char *fuyubot_last_error(void);

/* ---- 运行时查询 -------------------------------------------------- */

/*
 * 当前插件列表 + 计数（JSON）：
 *
 *   {
 *     "count": 2,            // 交给内核的插件数（= plugins 数组长度）
 *     "scanned": 3,          // 目录里扫到的候选数（= count + skipped）
 *     "active": 1,           // 五个状态计数，加起来恒等于 count
 *     "pending": 0,
 *     "activating": 0,
 *     "failed": 1,
 *     "unknown": 0,
 *     "skipped": 1,          // 没进内核的候选数（探测失败 / 位置不对的脚本）
 *     "plugins": [
 *       {
 *         "name": "fuyubot.start",           // 插件名（点分；plugin_init 报的）
 *         "version": "0.1.2",                // 插件 manifest 的版本；未写版本时为空串
 *         "abi_version": 5,                  // 插件编进去的 ABI 版本
 *         "state": "active",                 // active / pending / activating / failed / unknown
 *         "inject": ["fuyubot.host.td"],     // 硬依赖
 *         "soft_inject": [],                 // 软依赖
 *         "path": "/data/…/plugins/lib….so", // 插件文件（绝对路径）
 *         "file": "lib….so",                 // 文件名
 *         "missing": ["…"],                  // 仅 state = pending：还缺哪些能力
 *         "reason": "…"                      // 仅 state = failed：apply 报的错
 *       }
 *     ],
 *     "skipped_plugins": [                 // 连内核都没进的候选，附原因
 *       {"path": "/data/…/plugins/libbroken.so", "reason": "打开动态库失败 …"}
 *     ]
 *   }
 *
 * 运行时没起来 = 同一份形状的空表（count/scanned/… 全是 0，两个数组是 []），
 * 不是 NULL、也不是错误。
 * **可用时机**：插件装好就有（登录**之前**）——登录窗口弹出时就能查。
 * 返回线程局部缓冲：下次同线程调本库前有效。
 */
const char *fuyubot_plugins(void);

/*
 * 单个插件的信息（JSON，形状就是上面 plugins 数组里的一项）。
 * 没这个插件 / 运行时没起来 = NULL（**不设** last_error：找不到不是错）。
 * 返回线程局部缓冲。
 */
const char *fuyubot_plugin_info(const char *name);

/*
 * 发一条**原始 td_api JSON** 并等它自己的响应，返回响应 JSON（原样，
 * `{"@type":"error",…}` 也算响应，不是 NULL）。
 *
 *   const char *r = fuyubot_td_request("{\"@type\":\"getMe\"}", 30000);
 *
 * - timeout_ms <= 0 → 默认 30 秒；上限 600 秒（再长只钳住，不报错）。
 * - **阻塞**：从工作线程调。
 * - ⚠️ 别在 `onAsk` / `onState` 回调里调它：那些回调跑在**工作线程**上，
 *   等响应等于自己等自己。本库会认出这条线程并立刻返回 NULL（last_error 说明原因）。
 * - 失败（运行时没起来 / 请求非法 / 超时 / 发不出去）= NULL，细节看 last_error()。
 * - 返回线程局部缓冲。
 */
const char *fuyubot_td_request(const char *request_json, int32_t timeout_ms);

/*
 * ---- JNI 契约 ----
 *
 * 绑定在**运行期**做：`JNI_OnLoad` 里用 `RegisterNatives` 把十个方法绑到
 * `com.catmoecircle.fuyubot.runtime.Native`（默认值）。构建时给
 * `FUYUBOT_JNI_CLASS=com/你的/包/Native` 就能换包名——**必须是 `/` 分隔的内部名**，
 * 且换完不用改任何函数名（导出符号里根本没有 `Java_...`）。
 * 类名 / 方法名 / 签名必须照下面写：写错同样在第一次调用时报 `UnsatisfiedLinkError`。
 * 绑定是**逐个方法注册**的：某个方法在类里缺失只会让那一个绑不上（`JNI_OnLoad` 日志里
 * 列出名字），其它方法照常可用。
 *
 * public final class Native {
 *     public Native(Callback cb)               // 构造里先 loadLibrary("tdjson")，再 loadLibrary("fuyubotjni")
 *     public native int  nativeVersion();      // == FUYUBOT_JNI_ABI
 *     public native int  nativeRegister(Object cb);   // 注意参数类型是 Object，不是 Callback
 *     public native int  nativeStart(String optionsJson);
 *     public native int  nativeStop();
 *     public native boolean nativeIsRunning();
 *     public native int  nativeAnswer(int id, String text);   // text = null 表示取消
 *
 *     // ---- 运行时查询 ----
 *     public native String nativePlugins();                 // 插件列表 JSON；没起来 = 空表
 *     public native String nativePluginInfo(String name);   // 单个插件；没有 = null
 *     public native String nativeTdRequest(String requestJson, long timeoutMs); // 阻塞
 *     public native String nativeLastError();               // 本线程最后一次错误
 * }
 *
 * `nativeRegister` 在 JNI 层是 `(Ljava/lang/Object;)I`：回调只按**方法名 + 签名**调
 * （onState / onAsk / onQr / onPlugins），不认具体接口——所以 `Callback` 放哪个包都行，
 * Kotlin 里把参数声明成 `Any` 即可：
 *
 * public interface Callback {
 *     void onState(int state, String detail);   // 0 STARTING 1 READY 2 STOPPING 3 STOPPED 4 ERROR
 *     void onAsk(int id, int kind, String prompt);  // 回答走 Native.nativeAnswer(id, text)
 *     void onQr(String link);                       // 只展示，不需要回答
 *     void onPlugins(String reportJson);            // 可选：插件清单 + 计数 + 跳过原因
 * }
 *
 * `onPlugins` 是**可选**回调：装完插件、登录**之前**调一次，参数就是 `fuyubot_plugins()`
 * 那份 JSON（见上）；不需要时不必在 `Callback` 里声明。任何时候也都可以用
 * `nativePlugins()` 主动拉取。
 *
 * kind 取值：0 bot token / 1 手机号 / 2 验证码 / 3 两步密码 / 4 登录方式（回 "phone" 或 "qr"）/
 * 5 其它（把 prompt 原样显示给用户即可）。prompt 一律是给人看的中文整句。
 *
 * `nativeTdRequest` **阻塞**（按 timeoutMs 等 TDLib 响应，建议 `Dispatchers.IO`）；
 * `nativePlugins` 只是一次 JSON 序列化。它们返回 null / 空时，用 `nativeLastError()`
 * 看原因——注意它是**线程局部**的：要在同一个线程上紧接着读。
 * ⚠️ `onState` / `onAsk` 回调都在**工作线程**上：在回调里同步调 `nativeTdRequest`
 * 等于让收响应的人等自己，会立刻拿到 null（`nativeLastError()` 里写明原因）。
 *
 * R8 / ProGuard：**必须 keep 这个类**——绑定是运行期按名字做的，混淆后类名/方法名一变
 * 就绑不上（要到第一次调用才报 UnsatisfiedLinkError）：
 *   -keep class com.catmoecircle.fuyubot.runtime.Native { *; }
 *
 * 回调发生在**工作线程**（不是 Java 主线程）：Java 侧弹窗 / 通知请自行切线程；
 * 回调里别做长时间阻塞的事（会拖住登录状态机）。
 */

#ifdef __cplusplus
}
#endif

#endif /* FUYUBOT_JNI_H */
