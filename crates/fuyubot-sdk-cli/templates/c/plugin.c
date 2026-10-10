/*
 * __NAME__：fuyubot 插件（C）。
 *
 * 逻辑：收到 updateNewMessage，正文等于 `/ping` 就回 `pong`。
 * 由 `fuyubot-sdk init --lang c` 生成；用你自己的工具链编它（`gcc -shared` / `cl /LD`）。
 *
 * 编译时头文件 `fuyubot_plugin.h` 就在本目录（`fuyubot-sdk init` 写出来的）。
 */
#include <stdio.h>
#include <string.h>

#include "fuyubot_plugin.h"

/* 宿主 VTable 在本进程里地址稳定，init 时存下来，之后所有调用都经它走。 */
static const HostApiVTable *g_host = NULL;
/* PluginHandle::context 必须非空（判据是「版本非 0 且 context 非空」）。 */
static int g_state = 0;

/*
 * 极朴素的 JSON 取值：够样例用，**不处理转义、不处理同名嵌套键**。
 * 真要认真解析就引一个 JSON 库——这里刻意不引，好让模板保持零依赖。
 */
static int json_string(const char *json, const char *key, char *out, size_t cap) {
    char pattern[64];
    snprintf(pattern, sizeof(pattern), "\"%s\":\"", key);
    const char *start = strstr(json, pattern);
    if (start == NULL) {
        return 0;
    }
    start += strlen(pattern);
    const char *end = strchr(start, '"');
    if (end == NULL) {
        return 0;
    }
    size_t length = (size_t)(end - start);
    if (length + 1 > cap) {
        return 0;
    }
    memcpy(out, start, length);
    out[length] = '\0';
    return 1;
}

static long long json_int(const char *json, const char *key) {
    char pattern[64];
    snprintf(pattern, sizeof(pattern), "\"%s\":", key);
    const char *start = strstr(json, pattern);
    if (start == NULL) {
        return 0;
    }
    return strtoll(start + strlen(pattern), NULL, 10);
}

/* 回调和宿主在同一根线程上跑；回调必须是能 unwind 的 ABI（见头文件里的说明）。 */
static int32_t on_update(void *user_data, const char *payload) {
    (void)user_data;
    char text[256];
    if (!json_string(payload, "text", text, sizeof(text)) || strcmp(text, "/ping") != 0) {
        return 0;
    }
    long long chat_id = json_int(payload, "chat_id");
    if (chat_id == 0) {
        return 0;
    }
    char params[512];
    snprintf(params, sizeof(params), "{\"chat_id\":%lld,\"text\":\"pong\"}", chat_id);
    g_host->td_invoke_simplified(g_host->host_ctx, "sendMessage", params);
    g_host->log(g_host->host_ctx, 3, "ping 插件：回了 pong");
    return 0;
}

FUYUBOT_EXPORT PluginHandle plugin_init(const HostApiVTable *host) {
    PluginHandle failed = {0, 0, NULL, NULL};
    if (host == NULL || host->version != ABI_VERSION) {
        return failed; /* 版本不对就别往下走，宿主会把这个句柄当成加载失败。 */
    }
    g_host = host;
    PluginHandle handle = {
        ABI_VERSION,
        (uint32_t)sizeof(PluginHandle),
        &g_state,
        "__NAME__",
    };
    return handle;
}

FUYUBOT_EXPORT int32_t plugin_start(PluginHandle handle, const char *config_json) {
    (void)config_json;
    if (handle.context == NULL || g_host == NULL) {
        return -1;
    }
    SubscriptionHandle subscription =
        g_host->on(g_host->host_ctx, "updateNewMessage", NULL, on_update, NULL);
    return subscription.id == 0 ? -1 : 0;
}

FUYUBOT_EXPORT void plugin_stop(PluginHandle handle) {
    (void)handle;
    /* 订阅随插件卸载一起被宿主回滚；这里只把状态清掉。 */
    g_host = NULL;
}

FUYUBOT_EXPORT void plugin_free_string(char *ptr) {
    /* 本插件不往外送字符串；留着是为了满足 ABI（宿主可能调它）。 */
    (void)ptr;
}
