// __NAME__：fuyubot 插件（JS/TS）。
//
// 逻辑都写在这里。`fuyubot_plugin` 是桥（由 `fuyubot-sdk init` 一起放在本目录），
// 它把宿主的能力包成了普通函数，类型声明见同目录的 `fuyubot_plugin.d.ts`。
//
// 跑起来的方式：`fuyubot build-plugin` 编出 cdylib，`fuyubot run` 加载它，
// 宿主会替你启动 node 执行本文件。也可以直接 `node __SCRIPT__` 调试，
// 只是没有宿主可说话。

const host = require("./fuyubot_plugin.js");

host.on("updateNewMessage", (payload) => {
  host.log("__NAME__ 收到一条更新");
});

host.on("updateNewMessage", (payload) => {
  host.log("有人说 /hello");
}, { "message.content.text.text": "/hello" });

host.run();
