"""__NAME__：fuyubot 插件（Python）。

逻辑都写在这里。`fuyubot_plugin` 是桥（由 `fuyubot-sdk init` 一起放在本目录），
它把宿主的能力包成了普通函数，类型声明见同目录的 `fuyubot_plugin.pyi`。

跑起来的方式：`fuyubot build-plugin` 编出 cdylib，`fuyubot run` 加载它，
宿主会替你启动 python 解释器执行本文件。也可以直接 `python __SCRIPT__` 调试，
只是没有宿主可说话。
"""

import fuyubot_plugin as host


@host.on("updateNewMessage")
def on_message(payload):
    """收到一条消息。payload 是 TDLib 原始 JSON。"""
    host.log("__NAME__ 收到一条更新")


@host.on("updateNewMessage", filter={"message.content.text.text": "/hello"})
def on_hello(payload):
    """按字段过滤也可以在宿主那侧做（filter 里的其余键参与负载匹配）。"""
    host.log("有人说 /hello")


host.run()
