"use strict";
/**
 * fuyubot 脚本插件桥的 JS/TS 侧。
 *
 * 这不是业务逻辑，只是**契约映射**：把宿主 `HostApiVTable` 上的动作包成普通函数，
 * 底下走「一行一个 JSON」的协议（协议细节见 `fuyubot-sdk-script` 的 crate 文档）。
 *
 * 用法：把这个文件和你的 `plugin.js` 放在同一个目录，然后
 *
 *     const host = require("./fuyubot_plugin.js");
 *
 *     host.on("updateNewMessage", (payload) => host.log("收到一条更新"));
 *
 *     host.run();
 *
 * 类型声明见同目录的 `fuyubot_plugin.d.ts`。
 * Node 与 Bun 都跑得起来（宿主会依次尝试 `node`、`bun`）。
 */

const readline = require("readline");

const handlers = new Map(); // 宿主订阅号 -> [回调]
const pendingOns = new Map(); // tag -> 回调（等宿主把订阅号回执回来）
const pendingInvokes = new Map(); // tag -> 回调（等宿主把请求号回执回来）
const responseHandlers = new Map(); // request_id -> 回调
let responseChannelOpen = false;
let nextTag = 1;
let config = {};

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

function newTag() {
  return nextTag++;
}

/** 往宿主日志里写一行。`level`：error / warn / info / debug / trace。 */
function log(message, level = "info") {
  send({ op: "log", level, message: String(message) });
}

/**
 * 订阅事件。
 *
 * `filter` 里的 `event_type` 键是**选通道**用的（`"update"` / `"response"`），
 * 其余键做负载匹配。
 *
 * **同一个事件可以订阅多次**，各带各的过滤器：宿主按订阅号把事件分回来，
 * 这里也按订阅号分发，所以不会出现「过滤器没命中的回调也被调到」。
 */
function on(event, handler, filter) {
  const tag = newTag();
  pendingOns.set(tag, handler);
  const message = { op: "on", tag, event };
  if (filter !== undefined && filter !== null) {
    message.filter = filter;
  }
  send(message);
  return handler;
}

/** 退订（只影响本进程的分发表；宿主的订阅随插件停止一起撤掉）。 */
function off(event, handler) {
  for (const list of handlers.values()) {
    const index = list.indexOf(handler);
    if (index >= 0) {
      list.splice(index, 1);
    }
  }
}

/**
 * 发一条 TDLib 请求。
 *
 * 给了 `onResponse` 就会在这条请求的响应回来时调它——响应按 `request_id`
 * 配对，脚本不用自己记请求号。
 */
function invoke(method, params, onResponse) {
  ensureResponseChannel();
  const tag = newTag();
  if (typeof onResponse === "function") {
    pendingInvokes.set(tag, onResponse);
  }
  send({ op: "invoke", tag, method, params: params || {} });
  return tag;
}

/** 发一条自定义事件（其他插件订阅得到）。 */
function emit(event, payload) {
  send({ op: "emit", event, payload });
}

/** 登记一个工具的元数据。 */
function registerTool(name, description = "", inputSchema) {
  const descriptor = { name, description };
  if (inputSchema !== undefined && inputSchema !== null) {
    descriptor.input_schema = inputSchema;
  }
  send({ op: "tool", descriptor });
}

/** 宿主传给插件的配置（`plugin_start` 的 config JSON），没有就是空对象。 */
function getConfig() {
  return config;
}

function ensureResponseChannel() {
  // 响应按 request_id 配对，得先收得到它。
  if (!responseChannelOpen) {
    responseChannelOpen = true;
    on("response", deliverResponse);
  }
}

function dispatch(message) {
  if (message.op === "event") {
    // 按订阅号分发：同一个事件可能注册了带不同过滤器的多条订阅。
    const list = handlers.get(message.handle) || [];
    for (const handler of list.slice()) {
      handler(message.payload);
    }
  } else if (message.op === "on") {
    const handler = pendingOns.get(message.tag);
    if (handler) {
      pendingOns.delete(message.tag);
      if (!handlers.has(message.handle)) {
        handlers.set(message.handle, []);
      }
      handlers.get(message.handle).push(handler);
    }
  } else if (message.op === "invoked") {
    const handler = pendingInvokes.get(message.tag);
    if (handler) {
      pendingInvokes.delete(message.tag);
      responseHandlers.set(message.request_id, handler);
    }
  } else if (message.op === "error") {
    log("宿主报错：" + String(message.message), "error");
  }
}

function deliverResponse(payload) {
  if (payload === null || typeof payload !== "object") {
    return;
  }
  const handler = responseHandlers.get(payload.request_id);
  if (handler) {
    responseHandlers.delete(payload.request_id);
    handler(payload);
  }
}

/** 读宿主发来的行、分发给回调，直到宿主关掉管道。插件的最后一行就是它。
 *
 * 这一行必须在**所有 `on(...)` 之后**：宿主会等收到 `{"op":"ready"}` 才认为
 * 插件加载完成，而 `plugin_start` 也要等它——脚本早退会被判成加载失败。
 */
function run() {
  if (process.argv[2] && process.argv[2].trim()) {
    try {
      config = JSON.parse(process.argv[2]);
    } catch (error) {
      config = {};
    }
  }
  send({ op: "ready" });
  const lines = readline.createInterface({ input: process.stdin });
  lines.on("line", (line) => {
    const text = line.trim();
    if (!text) {
      return;
    }
    try {
      dispatch(JSON.parse(text));
    } catch (error) {
      log("收到一行不是 JSON 的内容：" + text, "error");
    }
  });
}

/** `node fuyubot_plugin.js` 就能跑。
 *
 * 验的是这一层最容易写错的两件事：**事件按订阅号分发**（不是按事件名，
 * 否则带过滤器的第二条订阅会在不该响的时候响），以及**没见过的 handle/tag
 * 不能把脚本搞挂**。
 */
function selfTest() {
  const seen = [];
  handlers.clear();
  handlers.set(1, [(payload) => seen.push(["a", payload])]);
  handlers.set(2, [(payload) => seen.push(["b", payload])]);
  dispatch({ op: "event", handle: 1, payload: { n: 1 } });
  dispatch({ op: "event", handle: 2, payload: { n: 2 } });
  // 没订阅过的 handle、没有 tag 的消息：静悄悄忽略，不许炸。
  dispatch({ op: "event", handle: 99, payload: { n: 3 } });
  dispatch({ op: "on", handle: 5 });
  assert(JSON.stringify(seen) === JSON.stringify([["a", { n: 1 }], ["b", { n: 2 }]]), seen);

  // 订阅回执要把回调挂到**回执里的那个 handle** 上。
  const tag = newTag();
  pendingOns.set(tag, (payload) => seen.push(["c", payload]));
  dispatch({ op: "on", tag, handle: 42 });
  dispatch({ op: "event", handle: 42, payload: { n: 4 } });
  assert(JSON.stringify(seen[seen.length - 1]) === JSON.stringify(["c", { n: 4 }]), seen);

  // 响应按 request_id 配对，且只喂一次。
  responseHandlers.set(7, (payload) => seen.push(["r", payload]));
  deliverResponse({ request_id: 7, ok: true });
  deliverResponse({ request_id: 7, ok: true });
  const responses = seen.filter((item) => item[0] === "r");
  assert(responses.length === 1, responses);

  console.log("fuyubot_plugin: self-test ok");
}

function assert(condition, detail) {
  if (!condition) {
    console.error("fuyubot_plugin: self-test FAILED", detail);
    process.exit(1);
  }
}

module.exports = {
  log,
  on,
  off,
  invoke,
  emit,
  registerTool,
  config: getConfig,
  run,
};

if (require.main === module) {
  selfTest();
}
