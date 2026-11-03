//! 运行期只读视图：`App` 留在 worker 线程上，**别的线程**（JNI 调用方、TUI）用它
//! 查插件、发 TDLib JSON 请求。
//!
//! 为什么不直接把 `App` 摊出去：`App` 攥着内核（`PluginManager` 要 `&mut`）、
//! 接收循环把手和收摊的所有权。把它共享给任意线程，等于把「谁在什么时候能做什么」
//! 搞糊。这份视图只包三样**本来就线程安全**的东西：
//! `Arc<HostRuntime>`、插件静态元信息（不可变）与 `Arc<TdClient>`。
//!
//! 插件状态是**现算**的：`HostRuntime` 里装着内核的 `PluginReporter`（弱引用），
//! 所以这里拿到的是这一刻的真实状态，而不是装插件那一刻的快照。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fuyubot_loader::HostRuntime;
use fuyubot_tdlib::TdClient;
use serde_json::{Value, json};

/// 一个已装插件的**静态**自描述（探测 / manifest 阶段就有，之后不会变）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// 插件名（点分；`plugin_init` 报的那个）。
    pub name: String,
    /// 插件自己的版本（ABI v4 的 `plugin_manifest`；老插件是空串）。
    pub version: String,
    /// 插件编进去的 ABI 版本。
    pub abi_version: u32,
    /// 硬依赖：缺了不激活（manifest 优先，配置段兜底）。
    pub inject: Vec<String>,
    /// 软依赖：缺了不阻塞，有了优先排前面。
    pub soft_inject: Vec<String>,
    /// 插件文件（绝对路径）。
    pub path: PathBuf,
}

/// 运行期只读视图。`Clone` 廉价（只克隆 `Arc`），可以随用随取。
#[derive(Clone)]
pub struct LiveView {
    runtime: Arc<HostRuntime>,
    plugins: Arc<Vec<PluginInfo>>,
    client: Option<Arc<TdClient>>,
}

impl LiveView {
    pub(crate) fn new(
        runtime: Arc<HostRuntime>,
        plugins: Vec<PluginInfo>,
        client: Option<Arc<TdClient>>,
    ) -> Self {
        Self {
            runtime,
            plugins: Arc::new(plugins),
            client,
        }
    }

    /// 插件列表 JSON：`{"count": n, "plugins": [ … ]}`。
    ///
    /// 每项的字段见 [`Self::plugin_json`]；状态取自内核的实时报告。
    pub fn plugins_json(&self) -> String {
        let plugins = self.entries();
        json!({ "count": plugins.len(), "plugins": plugins }).to_string()
    }

    /// 单个插件的 JSON；没这个插件返回 `None`。
    ///
    /// 形状（`missing` / `reason` 只在对应状态下出现）：
    ///
    /// ```json
    /// {
    ///   "name": "fuyubot.start",
    ///   "version": "0.1.0",
    ///   "abi_version": 5,
    ///   "state": "active",          // active / pending / activating / failed / unknown
    ///   "inject": ["fuyubot.host.td"],
    ///   "soft_inject": [],
    ///   "path": "/data/.../plugins/libfuyubot_start.so",
    ///   "file": "libfuyubot_start.so",
    ///   "missing": ["一些能力"],     // 仅 pending
    ///   "reason": "…"               // 仅 failed
    /// }
    /// ```
    ///
    /// `state` 为 `unknown` 只有一个原因：内核还没装 `PluginReporter`（比如测试里的
    /// 裸 `HostRuntime`）。真跑起来之后不会出现。
    pub fn plugin_json(&self, name: &str) -> Option<String> {
        self.entries()
            .into_iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
            .map(|entry| entry.to_string())
    }

    /// 发一条原始 td_api JSON，等它自己的响应。
    ///
    /// 参数与返回值都是 JSON **文本**：这条就是给「不想为每个方法生成一个绑定」的
    /// 宿主用的。`timeout` 由调用方给（JNI 侧有默认值与上限）。
    pub fn td_request(&self, request: &str, timeout: Duration) -> Result<String, String> {
        let value: Value =
            serde_json::from_str(request).map_err(|error| format!("请求不是合法 JSON：{error}"))?;
        if self.client.is_none() {
            return Err("这次运行没有接 TDLib（`--no-tdlib` / 回放，或还没建好客户端）".to_owned());
        }
        let response = self
            .runtime
            .td_request(value, timeout)
            .map_err(|error| error.to_string())?;
        Ok(response.to_string())
    }

    /// 插件数（静态表；与 `plugins_json` 的 `count` 一致）。
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// 所有插件的条目：静态元信息 + 内核状态。
    fn entries(&self) -> Vec<Value> {
        let live = self.live_states();
        let mut out: Vec<Value> = Vec::with_capacity(self.plugins.len());
        for info in self.plugins.iter() {
            let state = live.iter().find(|entry| {
                entry.get("name").and_then(Value::as_str) == Some(info.name.as_str())
            });
            out.push(Self::entry(info, state));
        }
        // 防御：内核报了、静态表里却没有的（理论上不该发生：两边同源），照实补上。
        for entry in &live {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !self.plugins.iter().any(|info| info.name == name) {
                out.push(entry.clone());
            }
        }
        out
    }

    /// 内核此刻的插件报告（`{"count":…,"plugins":[…]}` → `plugins` 数组）。
    fn live_states(&self) -> Vec<Value> {
        serde_json::from_str::<Value>(&self.runtime.plugin_report())
            .ok()
            .and_then(|report| report.get("plugins").and_then(Value::as_array).cloned())
            .unwrap_or_default()
    }

    fn entry(info: &PluginInfo, state: Option<&Value>) -> Value {
        let mut entry = json!({
            "name": info.name,
            "version": info.version,
            "abi_version": info.abi_version,
            "state": state
                .and_then(|entry| entry.get("state"))
                .cloned()
                .unwrap_or_else(|| json!("unknown")),
            "inject": info.inject,
            "soft_inject": info.soft_inject,
            "path": info.path.to_string_lossy(),
            "file": info
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        });
        if let Some(object) = entry.as_object_mut() {
            for key in ["missing", "reason"] {
                if let Some(value) = state.and_then(|entry| entry.get(key)) {
                    object.insert(key.to_owned(), value.clone());
                }
            }
        }
        entry
    }
}

impl std::fmt::Debug for LiveView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveView")
            .field("plugins", &self.plugins.len())
            .field("tdlib", &self.client.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_core::Context;

    fn info(name: &str, version: &str) -> PluginInfo {
        PluginInfo {
            name: name.to_owned(),
            version: version.to_owned(),
            abi_version: 5,
            inject: vec!["fuyubot.host.td".to_owned()],
            soft_inject: Vec::new(),
            path: PathBuf::from("/data/user/0/com.example.bot/files/plugins/libdemo.so"),
        }
    }

    /// 测试里的视图：裸 `HostRuntime`（没装 `PluginReporter`），也没有 TDLib。
    fn view(plugins: Vec<PluginInfo>) -> LiveView {
        let runtime = HostRuntime::new(Context::new()).expect("运行时应该建得起来");
        LiveView::new(runtime, plugins, None)
    }

    #[test]
    fn list_json_carries_metadata_and_marks_unknown_state() {
        let view = view(vec![
            info("fuyubot.demo", "0.2.0"),
            info("fuyubot.start", "0.2.0"),
        ]);
        let parsed: Value = serde_json::from_str(&view.plugins_json()).unwrap();

        assert_eq!(parsed["count"], json!(2));
        let first = &parsed["plugins"][0];
        assert_eq!(first["name"], json!("fuyubot.demo"));
        assert_eq!(first["version"], json!("0.2.0"));
        assert_eq!(first["abi_version"], json!(5));
        // 没有内核报告时**不编造状态**，如实说 unknown。
        assert_eq!(first["state"], json!("unknown"));
        assert_eq!(first["inject"], json!(["fuyubot.host.td"]));
        assert_eq!(first["file"], json!("libdemo.so"));
        assert!(first.get("reason").is_none(), "没失败就不该有 reason");
    }

    #[test]
    fn plugin_json_finds_one_and_returns_none_for_strangers() {
        let view = view(vec![info("fuyubot.demo", "0.2.0")]);
        let parsed: Value =
            serde_json::from_str(&view.plugin_json("fuyubot.demo").unwrap()).unwrap();
        assert_eq!(parsed["name"], json!("fuyubot.demo"));
        assert!(view.plugin_json("fuyubot.missing").is_none());
    }

    #[test]
    fn list_json_is_empty_but_well_formed_without_plugins() {
        let view = view(Vec::new());
        let parsed: Value = serde_json::from_str(&view.plugins_json()).unwrap();
        assert_eq!(parsed, json!({"count": 0, "plugins": []}));
        assert!(view.is_empty());
    }

    #[test]
    fn td_request_without_a_client_says_so() {
        let view = view(Vec::new());
        let error = view
            .td_request(r#"{"@type":"getMe"}"#, Duration::from_secs(1))
            .unwrap_err();
        assert!(error.contains("没有接 TDLib"), "{error}");
    }

    #[test]
    fn td_request_rejects_bad_json_before_touching_tdlib() {
        let view = view(Vec::new());
        let error = view
            .td_request("not json", Duration::from_secs(1))
            .unwrap_err();
        assert!(error.contains("不是合法 JSON"), "{error}");
    }
}
