# fuyubot-app

宿主应用的**编排层**：把「内核 → 宿主运行时 → 接 TDLib → 装插件 → 登录 → 分发 → 收摊」
这一条顺序写在一处，CLI（`fuyubot run` / `login`）与 Android 的 `libfuyubotjni.so` 共用。

它不认识 TUI、不认识 clap、不认识 JNI：交互全部走 [`AppHost`](src/host.rs) 这一个 trait
（`state` / `ask` / `qr`），由各宿主自己实现——控制台弹问题、终端面板弹输入框、JNI 弹 Java 回调。

与上下层的关系：

```text
fuyubot-core    内核（Context / Effect / PluginManager）
fuyubot-loader  宿主运行时、VTable、插件加载、更新管线
fuyubot-tdlib   TDLib 加载、接收循环、授权状态机、Session
     └──────── fuyubot-app（本 crate）：把上面三者按**顺序**接起来
                    ├── fuyubot-cli（bin）：控制台 + TUI + 实例锁 / status 文件
                    └── fuyubot-android（cdylib）：JNI 导出
```
