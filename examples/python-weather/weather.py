"""weather 示例插件：`/weather <城市>` 去外部 API 查天气，再把结果回过去。

逻辑全在这里（Python），ABI 胶水在 `src/lib.rs`（Rust）。
用法：

    /weather 上海

数据来自 wttr.in（公开、不用 API key）。查不到就把错误原样回过去——
插件不该因为一次网络失败把宿主拖下水。
"""

import urllib.error
import urllib.parse
import urllib.request

import fuyubot_plugin as host

COMMAND = "/weather"
DEFAULT_CITY = "Beijing"
TIMEOUT_SECONDS = 10


def message_text(message):
    """从 TDLib 的 message 对象里取纯文本正文。"""
    content = message.get("content") or {}
    if content.get("@type") != "messageText":
        return ""
    return (content.get("text") or {}).get("text", "")


def fetch_weather(city):
    """查天气。失败时返回一句人话，而不是抛出去。"""
    url = "https://wttr.in/" + urllib.parse.quote(city) + "?format=3&lang=zh"
    request = urllib.request.Request(url, headers={"User-Agent": "curl/8"})
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            return response.read().decode("utf-8", "replace").strip()
    except (urllib.error.URLError, OSError, ValueError) as error:
        return "查不到 %s 的天气：%s" % (city, error)


def reply(chat_id, text):
    host.invoke("sendMessage", {"chat_id": chat_id, "text": text})


@host.on("updateNewMessage")
def on_message(payload):
    message = (payload or {}).get("message") or {}
    if message.get("is_outgoing"):
        return  # 别回自己的消息，否则会自激
    text = message_text(message)
    if not text.startswith(COMMAND):
        return
    chat_id = message.get("chat_id")
    if not chat_id:
        return

    city = text[len(COMMAND):].strip() or DEFAULT_CITY
    host.log("查 %s 的天气" % city)
    reply(chat_id, fetch_weather(city))


host.run()
