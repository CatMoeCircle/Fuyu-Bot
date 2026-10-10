/*
 * __NAME__：fuyubot 插件（C++）。
 *
 * 逻辑：收到 updateNewMessage，正文等于 `/ping` 就回 `pong`。
 * 由 `fuyubot-sdk init --lang cpp` 生成；用你自己的工具链编它（`g++ -shared` / `cl /LD`）。
 *
 * 四个导出符号必须放在 `extern "C"` 里——C++ 会改函数名，宿主就 dlsym 不到了。
 */
#include <cstdint>
#include <cstring>
#include <string>

#include "fuyubot_plugin.h"

namespace {

const HostApiVTable *g_host = nullptr;
int g_state = 0;

/* 极朴素的 JSON 取值：够样例用，不处理转义。真要用请引一个 JSON 库。 */
std::string find_string(const std::string &json, const std::string &key) {
    const std::string pattern = "\"" + key + "\":\"";
    const std::size_t start = json.find(pattern);
    if (start == std::string::npos) {
        return {};
    }
    const std::size_t from = start + pattern.size();
    const std::size_t end = json.find('"', from);
    if (end == std::string::npos) {
        return {};
    }
    return json.substr(from, end - from);
}

long long find_int(const std::string &json, const std::string &key) {
    const std::string pattern = "\"" + key + "\":";
    const std::size_t start = json.find(pattern);
    if (start == std::string::npos) {
        return 0;
    }
    return std::strtoll(json.c_str() + start + pattern.size(), nullptr, 10);
}

int32_t on_update(void *user_data, const char *payload) {
    (void)user_data;
    const std::string json = payload == nullptr ? std::string() : std::string(payload);
    if (find_string(json, "text") != "/ping") {
        return 0;
    }
    const long long chat_id = find_int(json, "chat_id");
    if (chat_id == 0) {
        return 0;
    }
    const std::string params =
        "{\"chat_id\":" + std::to_string(chat_id) + ",\"text\":\"pong\"}";
    g_host->td_invoke_simplified(g_host->host_ctx, "sendMessage", params.c_str());
    g_host->log(g_host->host_ctx, 3, "ping 插件：回了 pong");
    return 0;
}

}  // namespace

extern "C" {

FUYUBOT_EXPORT PluginHandle plugin_init(const HostApiVTable *host) {
    PluginHandle failed{0, 0, nullptr, nullptr};
    if (host == nullptr || host->version != ABI_VERSION) {
        return failed;
    }
    g_host = host;
    PluginHandle handle{
        ABI_VERSION,
        static_cast<std::uint32_t>(sizeof(PluginHandle)),
        &g_state,
        "__NAME__",
    };
    return handle;
}

FUYUBOT_EXPORT int32_t plugin_start(PluginHandle handle, const char *config_json) {
    (void)config_json;
    if (handle.context == nullptr || g_host == nullptr) {
        return -1;
    }
    const SubscriptionHandle subscription =
        g_host->on(g_host->host_ctx, "updateNewMessage", nullptr, on_update, nullptr);
    return subscription.id == 0 ? -1 : 0;
}

FUYUBOT_EXPORT void plugin_stop(PluginHandle handle) {
    (void)handle;
    g_host = nullptr;
}

FUYUBOT_EXPORT void plugin_free_string(char *ptr) { (void)ptr; }

}  // extern "C"
