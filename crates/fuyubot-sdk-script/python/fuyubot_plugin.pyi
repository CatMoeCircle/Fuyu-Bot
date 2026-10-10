"""`fuyubot_plugin` 的类型声明（Python 插件作者对着它写代码）。

每个函数对应宿主 `HostApiVTable` 上的哪一项写在各自的 docstring 里——
**这就是「Python SDK 声明的 ABI 契约」**，宿主侧的对应实现见
`crates/fuyubot-loader/src/runtime.rs`。
"""

from typing import Any, Callable, Dict, List, Optional, Union

#: 负载/回调里能看到的 JSON 值。
Json = Union[None, bool, int, float, str, List[Any], Dict[str, Any]]
#: 事件回调：收到一条事件的负载。
Handler = Callable[[Json], None]

def log(message: str, level: str = "info") -> None:
    """宿主 `HostApiVTable::log`。`level`：error / warn / info / debug / trace。"""
    ...

def on(
    event: str,
    handler: Optional[Handler] = None,
    filter: Optional[Dict[str, Any]] = None,
) -> Any:
    """宿主 `HostApiVTable::on`。可当装饰器用。

    `filter` 里的 `event_type` 键选择通道（`"update"` / `"response"`），
    其余键做负载字段匹配；不参与匹配的键不会传给宿主。
    """
    ...

def off(event: str, handler: Handler) -> None:
    """host `HostApiVTable::off` 的本地一半：摘掉回调分发表里的一项。"""
    ...

def invoke(
    method: str,
    params: Optional[Dict[str, Any]] = None,
    on_response: Optional[Handler] = None,
) -> int:
    """宿主 `HostApiVTable::td_invoke_simplified`。

    `method` 是 TDLib 的方法名（如 `"sendMessage"`），`params` 是除 `@type`
    之外的参数。返回脚本侧的 tag；给了 `on_response` 就按 `request_id` 配对回调。
    """
    ...

def emit(event: str, payload: Optional[Json] = None) -> None:
    """宿主 `HostApiVTable::emit`。"""
    ...

def register_tool(
    name: str,
    description: str = "",
    input_schema: Optional[Dict[str, Any]] = None,
) -> None:
    """宿主 `HostApiVTable::register_tool`（只登记元数据，没有处理器指针）。"""
    ...

def config() -> Dict[str, Any]:
    """宿主传给插件的配置（`plugin_start` 的 config JSON）。"""
    ...

def run() -> None:
    """开始读宿主的行并分发。插件的最后一行永远是这个。"""
    ...
