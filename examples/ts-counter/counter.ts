/**
 * counter 示例插件：数每个聊天收到多少条消息，然后回一条「第 N 条」。
 *
 * 逻辑全在**这里**（TypeScript），ABI 胶水在 `src/lib.rs`（Rust）。
 * `fuyubot_plugin.js` / `fuyubot_plugin.d.ts` 是绑定（`fuyubot-sdk codegen --lang js` 的产物），
 * 与 `fuyubot-sdk-script` 里那份**逐字相同**——有测试盯着它别漂移。
 *
 * 用法：往任意聊天里发一条文本消息，就回一条「本聊天第 N 条消息」。
 * 计数是**按聊天分开**的，所以在同一个聊天里连发几条，N 会 1、2、3……递增。
 *
 * 怎么跑起来：宿主加载 `counter.dll`，由它启动解释器执行本文件。
 *
 *     fuyubot-sdk init 不是必需的（这是示例，源码已经在仓库里）
 *     cargo build -p fuyubot-example-counter          # 编出 counter.dll
 *     fuyubot run --replay examples/replay/counter.json --plugin-dir <放 dll + .ts 的目录>
 *
 * ⚠️ **TypeScript 是怎么被执行的**：宿主起的是 `node counter.ts`，靠 Node 自带的能力
 * **剥掉类型**再跑（Node ≥ 23.6 默认开启；22.6+ 要加 `--experimental-strip-types`）。
 * 所以本文件只能用「能剥掉」的语法：注解、`interface`/`type`、`as`，
 * **不能用** `enum` / `namespace` / 构造器参数属性（那些需要真正的编译）。
 * 你的 Node 太老就自己 `tsc` 编成 `.js`，或者把引擎换掉——反正插件只管逻辑，
 * 打包与编译是作者自己的工具链（见 `docs/ROADMAP.md` 变更 5）。
 */

import * as host from "./fuyubot_plugin.js";

/** 每个聊天收到过多少条（chat_id -> 计数）。 */
const counts = new Map<number, number>();

/**
 * `payload` 的类型是 `host.Json`（递归联合），取字段前先窄化成对象。
 *
 * JS 侧没有 schema，这就是代价：多两行窄化，换掉满地的 `any`。
 */
function object(value: host.Json): { [key: string]: host.Json } {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function text(value: host.Json): string {
  return typeof value === "string" ? value : "";
}

host.on("updateNewMessage", (payload) => {
  const message = object(object(payload)["message"]);

  // 别回自己发的消息，否则会自激（两个示例都踩过这个坑）。
  if (message["is_outgoing"] === true) return;

  const content = object(message["content"]);
  if (content["@type"] !== "messageText") return;
  const body = text(object(content["text"])["text"]);
  if (body === "") return;

  const chatId = message["chat_id"];
  if (typeof chatId !== "number") return;

  const count = (counts.get(chatId) ?? 0) + 1;
  counts.set(chatId, count);

  host.log(`${chatId} 的第 ${count} 条：${body}`);
  host.invoke("sendMessage", {
    chat_id: chatId,
    text: `本聊天第 ${count} 条消息`,
  });
});

host.run();
