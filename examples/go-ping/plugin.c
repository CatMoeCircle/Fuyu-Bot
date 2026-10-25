/*
 * go-ping 的 ABI 薄壳：四个导出符号在这里，逻辑全在 main.go。
 *
 * 编译：cgo 会把本文件和 main.go 一起编（`go build -buildmode=c-shared`），
 * `_cgo_export.h` 是 cgo 生成的，用来在 C 里调 Go 的 `go_on_update` 等函数。
 */
#include <stdint.h>
#include <string.h>

#include "fuyubot_plugin.h"
#include "_cgo_export.h"

/* 宿主 VTable 在本进程里地址稳定，init 时存下来。 */
static const HostApiVTable *g_host = NULL;
/* PluginHandle::context 必须非空（判据是「版本非 0 且 context 非空」）。 */
static int g_state = 0;
/* 订阅句柄要活着：drop 就是退订，所以存在模块级。 */
static SubscriptionHandle g_subscription = {0};

/* ---- 给 Go 用的两个小桥 -------------------------------------------------- */

void fy_log(const char *message) {
    if (g_host != NULL) {
        g_host->log(g_host->host_ctx, 3, message);
    }
}

long long fy_invoke(const char *method_name, const char *params_json) {
    if (g_host == NULL) {
        return 0;
    }
    return (long long)g_host->td_invoke_simplified(g_host->host_ctx, method_name, params_json).id;
}

/* ---- 宿主回调 → Go ------------------------------------------------------- */

static int32_t on_update(void *user_data, const char *payload) {
    return (int32_t)go_on_update(user_data, (char *)payload);
}

/* ---- 四个 ABI 符号 ------------------------------------------------------- */

FUYUBOT_EXPORT PluginHandle plugin_init(const HostApiVTable *host) {
    PluginHandle failed = {0, 0, NULL, NULL};
    if (host == NULL || host->version != ABI_VERSION) {
        return failed;
    }
    g_host = host;
    PluginHandle handle = {
        ABI_VERSION,
        (uint32_t)sizeof(PluginHandle),
        &g_state,
        "fuyubot.example.go-ping",
    };
    return handle;
}

FUYUBOT_EXPORT int32_t plugin_start(PluginHandle handle, const char *config_json) {
    if (handle.context == NULL || g_host == NULL) {
        return -1;
    }
    if (go_start((char *)config_json) != 0) {
        return -1;
    }
    g_subscription = g_host->on(g_host->host_ctx, "updateNewMessage", NULL, on_update, NULL);
    return g_subscription.id == 0 ? -1 : 0;
}

FUYUBOT_EXPORT void plugin_stop(PluginHandle handle) {
    (void)handle;
    go_stop();
    /* 订阅随插件卸载一起被宿主回滚；这里只把状态清掉。 */
    g_subscription.id = 0;
    g_host = NULL;
}

FUYUBOT_EXPORT void plugin_free_string(char *ptr) {
    (void)ptr;
}
