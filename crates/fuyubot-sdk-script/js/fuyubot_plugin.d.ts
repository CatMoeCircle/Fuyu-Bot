/**
 * `fuyubot_plugin` 的类型声明（TS/JS 插件作者对着它写代码）。
 *
 * 每个函数对应宿主 `HostApiVTable` 上的哪一项写在各自的注释里——
 * **这就是「JS SDK 声明的 ABI 契约」**，宿主侧的对应实现见
 * `crates/fuyubot-loader/src/runtime.rs`。
 *
 * 对应的实现是同目录的 `fuyubot_plugin.js`（CommonJS）；TS 里这样接：
 * `import * as host from "./fuyubot_plugin";`
 */

/** 负载/回调里能看到的 JSON 值。 */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** 事件回调：收到一条事件的负载。 */
export type Handler = (payload: Json) => void;

/** 宿主 `HostApiVTable::log`。`level`：error / warn / info / debug / trace。 */
export declare function log(message: string, level?: string): void;

/**
 * 宿主 `HostApiVTable::on`。
 *
 * `filter` 里的 `event_type` 键选择通道（`"update"` / `"response"`），
 * 其余键做负载字段匹配。
 */
export declare function on(
  event: string,
  handler: Handler,
  filter?: { [key: string]: Json },
): Handler;

/** 宿主 `HostApiVTable::off` 的本地一半：摘掉分发表里的一项。 */
export declare function off(event: string, handler: Handler): void;

/**
 * 宿主 `HostApiVTable::td_invoke_simplified`。
 *
 * `method` 是 TDLib 的方法名（如 `"sendMessage"`），`params` 是除 `@type`
 * 之外的参数。返回脚本侧的 tag；给了 `onResponse` 就按 `request_id` 配对回调。
 */
export declare function invoke(
  method: string,
  params?: { [key: string]: Json },
  onResponse?: Handler,
): number;

/** 宿主 `HostApiVTable::emit`。 */
export declare function emit(event: string, payload?: Json): void;

/** 宿主 `HostApiVTable::register_tool`（只登记元数据，没有处理器指针）。 */
export declare function registerTool(
  name: string,
  description?: string,
  inputSchema?: { [key: string]: Json },
): void;

/** 宿主传给插件的配置（`plugin_start` 的 config JSON）。 */
export declare function config(): { [key: string]: Json };

/** 开始读宿主的行并分发。插件的最后一行永远是这个。 */
export declare function run(): void;
