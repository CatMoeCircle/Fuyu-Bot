"""fuyubot 脚本插件桥的 Python 侧。

这不是业务逻辑，只是**契约映射**：把宿主 `HostApiVTable` 上的动作包成普通函数，
底下走「一行一个 JSON」的协议（协议细节见 `fuyubot-sdk-script` 的 crate 文档）。

用法：把这个文件和你的 `plugin.py` 放在同一个目录，然后

    import fuyubot_plugin as host

    @host.on("updateNewMessage")
    def on_message(payload):
        host.log("收到一条更新")

    host.run()

类型声明见同目录的 `fuyubot_plugin.pyi`。
"""

import json
import sys
import threading

# 宿主按 **UTF-8** 收发（一行一个 JSON）。Windows 上 Python 默认用系统代码页读管道，
# 中文会直接变成乱码（`上海` → `涓婃捣`），所以这里把三个流显式钉成 UTF-8，
# 不指望 PYTHONUTF8 / PYTHONIOENCODING 一定被设上。
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, ValueError, OSError):
        # 被换成非文本流、或已被 detach：那就按原样用，不因为编码问题把插件搞挂。
        pass

_stdout_lock = threading.Lock()
_handlers = {}            # 宿主订阅号 -> [回调]
_pending_ons = {}         # tag -> 回调（等宿主把订阅号回执给我们）
_pending_invokes = {}     # tag -> 回调（等宿主把请求号回执给我们）
_response_handlers = {}   # request_id -> 回调
_response_channel = [False]
_next_tag = 1
_config = {}


def _send(message):
    with _stdout_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def _new_tag():
    global _next_tag
    tag = _next_tag
    _next_tag += 1
    return tag


def log(message, level="info"):
    """往宿主日志里写一行。`level`：error / warn / info / debug / trace。"""
    _send({"op": "log", "level": level, "message": str(message)})


def on(event, handler=None, filter=None):
    """订阅事件。`handler` 给了就是普通调用，没给就能当装饰器用。

    `filter` 里的 `event_type` 键是**选通道**用的（"update" / "response"），
    其余键做负载匹配。

    **同一个事件可以订阅多次**，各带各的过滤器：宿主按订阅号把事件分回来，
    所以这里也是按订阅号分发的，不会出现「过滤器没命中的回调也被调到」。
    """
    if handler is None:
        def decorator(func):
            on(event, func, filter=filter)
            return func
        return decorator

    tag = _new_tag()
    _pending_ons[tag] = handler
    message = {"op": "on", "tag": tag, "event": event}
    if filter is not None:
        message["filter"] = filter
    _send(message)
    return handler


def off(event, handler):
    """退订（只影响本进程里的回调分发表；宿主的订阅随插件停止一起撤掉）。"""
    for handle in list(_handlers):
        handlers = _handlers[handle]
        if handler in handlers:
            handlers.remove(handler)


def invoke(method, params=None, on_response=None):
    """发一条 TDLib 请求。

    给了 `on_response` 就会在这条请求的响应回来时调它——响应按 `request_id`
    配对，脚本不用自己记请求号。
    """
    _ensure_response_channel()
    tag = _new_tag()
    if on_response is not None:
        _pending_invokes[tag] = on_response
    _send({"op": "invoke", "tag": tag, "method": method, "params": params or {}})
    return tag


def emit(event, payload=None):
    """发一条自定义事件（其他插件订阅得到）。"""
    _send({"op": "emit", "event": event, "payload": payload})


def register_tool(name, description="", input_schema=None):
    """登记一个工具的元数据。"""
    descriptor = {"name": name, "description": description}
    if input_schema is not None:
        descriptor["input_schema"] = input_schema
    _send({"op": "tool", "descriptor": descriptor})


def config():
    """宿主传给插件的配置（`plugin_start` 的 config JSON），没有就是空字典。"""
    return _config


def _ensure_response_channel():
    """确保订阅了 response 通道：响应按 request_id 配对，得先收得到它。"""
    if not _response_channel[0]:
        _response_channel[0] = True
        on("response", _deliver_response)


def _dispatch(message):
    op = message.get("op")
    if op == "event":
        # 按订阅号分发：脚本可能给同一个事件注册了带不同过滤器的多条订阅。
        for handler in list(_handlers.get(message.get("handle"), [])):
            handler(message.get("payload"))
    elif op == "on":
        handler = _pending_ons.pop(message.get("tag"), None)
        if handler is not None:
            _handlers.setdefault(message.get("handle"), []).append(handler)
    elif op == "invoked":
        handler = _pending_invokes.pop(message.get("tag"), None)
        if handler is not None:
            _response_handlers[message.get("request_id")] = handler
    elif op == "error":
        log("宿主报错：" + str(message.get("message")), level="error")


def _deliver_response(payload):
    if not isinstance(payload, dict):
        return
    handler = _response_handlers.pop(payload.get("request_id"), None)
    if handler is not None:
        handler(payload)


def run():
    """读宿主发来的行、分发给回调，直到宿主关掉管道。插件的最后一行就是它。

    这一行必须在**所有 `on(...)` 之后**：宿主会等收到 `{"op":"ready"}` 才认为
    插件加载完成，而 `plugin_start` 也要等它——脚本早退会被判成加载失败。
    """
    global _config
    if len(sys.argv) > 2 and sys.argv[2].strip():
        try:
            _config = json.loads(sys.argv[2])
        except ValueError:
            _config = {}

    _send({"op": "ready"})
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            _dispatch(json.loads(line))
        except ValueError:
            log("收到一行不是 JSON 的内容：" + line, level="error")


def _selftest():
    """`python fuyubot_plugin.py` 就能跑。

    验的是这一层最容易写错的两件事：**事件按订阅号分发**（不是按事件名，
    否则带过滤器的第二条订阅会在不该响的时候响），以及**没见过的 handle/tag
    不能把脚本搞挂**。
    """
    global _handlers
    seen = []
    _handlers = {
        1: [lambda payload: seen.append(("a", payload))],
        2: [lambda payload: seen.append(("b", payload))],
    }
    _dispatch({"op": "event", "handle": 1, "payload": {"n": 1}})
    _dispatch({"op": "event", "handle": 2, "payload": {"n": 2}})
    # 没订阅过的 handle、没有 tag 的消息：静悄悄忽略，不许炸。
    _dispatch({"op": "event", "handle": 99, "payload": {"n": 3}})
    _dispatch({"op": "on", "handle": 5})
    assert seen == [("a", {"n": 1}), ("b", {"n": 2})], seen

    # 订阅回执要把回调挂到**回执里的那个 handle** 上。
    tag = _new_tag()
    _pending_ons[tag] = lambda payload: seen.append(("c", payload))
    _dispatch({"op": "on", "tag": tag, "handle": 42})
    _dispatch({"op": "event", "handle": 42, "payload": {"n": 4}})
    assert seen[-1] == ("c", {"n": 4}), seen

    # 响应按 request_id 配对。
    _response_handlers[7] = lambda payload: seen.append(("r", payload))
    _deliver_response({"request_id": 7, "ok": True})
    _deliver_response({"request_id": 7, "ok": True})  # 已经消费掉了，不该再来一次
    assert seen[-1] == ("r", {"request_id": 7, "ok": True}), seen
    assert len([item for item in seen if item[0] == "r"]) == 1, seen

    print("fuyubot_plugin: self-test ok")


if __name__ == "__main__":
    _selftest()
