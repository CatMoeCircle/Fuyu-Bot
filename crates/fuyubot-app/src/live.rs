//! 运行期只读视图：`App` 留在 worker 线程上，**别的线程**（JNI 调用方、TUI）用它
//! 查插件、收 TDLib 的状态、发 TDLib JSON 请求。
//!
//! 为什么不直接把 `App` 摊出去：`App` 攥着内核（`PluginManager` 要 `&mut`）、
//! 接收循环把手和收摊的所有权。把它共享给任意线程，等于把「谁在什么时候能做什么」
//! 搞糊。这份视图只包四样**本来就线程安全**的东西：
//! `Arc<HostRuntime>`、插件静态元信息（不可变）、`Arc<TdClient>` 与
//! 授权 / 连接状态的快照（[`TdState`]）。
//!
//! 插件状态是**现算**的：`HostRuntime` 里装着内核的 `PluginReporter`（弱引用），
//! 所以这里拿到的是这一刻的真实状态，而不是装插件那一刻的快照。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fuyubot_loader::HostRuntime;
use fuyubot_tdlib::TdClient;
use serde_json::{Value, json};

use crate::tdstate::TdState;

/// 一个已装插件的**静态**自描述（探测 / manifest 阶段就有，之后不会变）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// 插件名（点分；`plugin_init` 报的那个）。
    pub name: String,
    /// 插件自己的版本（ABI v4 的 `plugin_manifest`；老插件是空串）。
    pub version: String,
    /// 给人看的名字（`plugin_manifest` 的 `display_name` / 清单的 `name`；空 = 用 `name`）。
    pub display_name: String,
    /// 一句话介绍（`plugin_manifest` 的 `description` / 清单的 `description`；空 = 不显示）。
    pub description: String,
    /// 插件编进去的 ABI 版本。
    pub abi_version: u32,
    /// 硬依赖：缺了不激活（manifest 优先，配置段兜底）。
    pub inject: Vec<String>,
    /// 软依赖：缺了不阻塞，有了优先排前面。
    pub soft_inject: Vec<String>,
    /// 插件文件（绝对路径）。
    pub path: PathBuf,
}

/// 一个**没进内核**就被拒掉的候选：探测失败（不是插件 / ABI 不符 / 打不开）或放错位置的脚本。
///
/// 它不会出现在 [`LiveView::plugins_json`] 的 `plugins` 数组里（那里只有真正交给内核的），
/// 但会出现在 `skipped_plugins` 里——「文件明明在那儿，列表里却没有」必须能查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSkip {
    /// 展示路径：目录包是那个目录，旧形态是那个动态库。
    pub path: PathBuf,
    /// 为什么跳过（给人看的中文整句，来自加载器 / 宿主自己的判断）。
    pub reason: String,
}

/// 一次插件目录扫描的结果：扫到几个、交给内核几个、哪些没进去。
///
/// 由 [`crate::App::load_plugins`] 产出，之后**不再变**（插件运行期的状态另由内核的
/// `PluginReporter` 现算，见 [`LiveView`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginLoadReport {
    /// 目录里扫到的候选数（目录包 + 旧形态平铺动态库）。
    pub scanned: usize,
    /// 没进内核的候选，按扫描顺序。
    pub skipped: Vec<PluginSkip>,
}

impl PluginLoadReport {
    /// 记一个跳过的候选。`scanned` 由调用方在扫描前填好，这里只追加原因。
    pub(crate) fn skip(&mut self, path: PathBuf, reason: String) {
        self.skipped.push(PluginSkip { path, reason });
    }

    /// `skipped_plugins` 的 JSON 形态：`[{"path": …,"reason": …}]`。
    fn skipped_json(&self) -> Vec<Value> {
        self.skipped
            .iter()
            .map(|skip| {
                json!({
                    "path": skip.path.to_string_lossy(),
                    "reason": skip.reason,
                })
            })
            .collect()
    }
}

/// 运行期只读视图。`Clone` 廉价（只克隆 `Arc`），可以随用随取。
#[derive(Clone)]
pub struct LiveView {
    runtime: Arc<HostRuntime>,
    plugins: Arc<Vec<PluginInfo>>,
    load: Arc<PluginLoadReport>,
    client: Option<Arc<TdClient>>,
    td_state: TdState,
}

impl LiveView {
    pub(crate) fn new(
        runtime: Arc<HostRuntime>,
        plugins: Vec<PluginInfo>,
        load: PluginLoadReport,
        client: Option<Arc<TdClient>>,
        td_state: TdState,
    ) -> Self {
        Self {
            runtime,
            plugins: Arc::new(plugins),
            load: Arc::new(load),
            client,
            td_state,
        }
    }

    /// 插件列表 JSON（形状见 [`Self::plugin_json`] 与 [`Self::load_report`]）：
    ///
    /// ```json
    /// {
    ///   "count": 2,            // 交给内核的插件数（= plugins 数组长度）
    ///   "scanned": 3,          // 目录里扫到的候选数（= count + skipped）
    ///   "active": 1, "pending": 0, "activating": 0, "failed": 1, "unknown": 0,
    ///   "skipped": 1,          // 没进内核的候选数
    ///   "plugins": [ … ],
    ///   "skipped_plugins": [ { "path": "…", "reason": "…" } ]
    /// }
    /// ```
    ///
    /// 五个状态计数之和恒等于 `count`；每项的字段见 [`Self::plugin_json`]，
    /// 状态取自内核的实时报告。`skipped_plugins` 是**扫描结果**，运行期不会变。
    pub fn plugins_json(&self) -> String {
        report_json(self.entries(), &self.load)
    }

    /// 这次扫描的结果（扫到多少、哪些没进内核）。
    pub fn load_report(&self) -> &PluginLoadReport {
        &self.load
    }

    /// 单个插件的 JSON；没这个插件返回 `None`。
    ///
    /// 形状（`missing` / `reason` 只在对应状态下出现）：
    ///
    /// ```json
    /// {
    ///   "name": "fuyubot.start",
    ///   "version": "0.1.0",
    ///   "display_name": "启动问候",
    ///   "description": "回复 /start 的欢迎语",
    ///   "abi_version": 6,
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
    /// `display_name` / `description` 空串表示插件没提供展示元数据（老插件），
    /// 展示方应当回退到 `name`。
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

    /// TDLib 授权状态 JSON（Android 的 `nativeAuthState()` 就是它）。
    ///
    /// 形状见 [`TdState::auth_json`]：`known` / `state` / `ready` / `needs_re_login`。
    pub fn auth_state_json(&self) -> String {
        self.td_state.auth_json()
    }

    /// TDLib 连接状态 JSON（Android 的 `nativeConnectionState()` 就是它）。
    ///
    /// 形状见 [`TdState::connection_json`]：`known` / `state` / `ready`。
    pub fn connection_state_json(&self) -> String {
        self.td_state.connection_json()
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
            "display_name": info.display_name,
            "description": info.description,
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

/// 把「插件条目 + 扫描结果」序列化成对外 JSON。
///
/// 抽成自由函数是为了让**空表**（运行时还没起来）与正常返回**同一份形状**：
/// [`empty_plugins_json`] 走的就是这里，客户端不用为「还没起来」写第二套解析。
fn report_json(plugins: Vec<Value>, load: &PluginLoadReport) -> String {
    let mut counts = std::collections::BTreeMap::new();
    for entry in &plugins {
        let state = entry
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        *counts.entry(state.to_owned()).or_insert(0usize) += 1;
    }
    let count_of = |state: &str| counts.get(state).copied().unwrap_or(0);
    json!({
        "count": plugins.len(),
        "scanned": load.scanned,
        "active": count_of("active"),
        "pending": count_of("pending"),
        "activating": count_of("activating"),
        "failed": count_of("failed"),
        "unknown": count_of("unknown"),
        "skipped": load.skipped.len(),
        "plugins": plugins,
        "skipped_plugins": load.skipped_json(),
    })
    .to_string()
}

/// 运行时没起来时的插件报告：空表，但**形状与正常返回一致**（不是 `null`，也不是错误）。
pub fn empty_plugins_json() -> String {
    report_json(Vec::new(), &PluginLoadReport::default())
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
            display_name: String::new(),
            description: String::new(),
            abi_version: fuyubot_loader::abi::ABI_VERSION,
            inject: vec!["fuyubot.host.td".to_owned()],
            soft_inject: Vec::new(),
            path: PathBuf::from("/data/user/0/com.example.bot/files/plugins/libdemo.so"),
        }
    }

    /// 测试里的视图：裸 `HostRuntime`（没装 `PluginReporter`），也没有 TDLib。
    fn view(plugins: Vec<PluginInfo>) -> LiveView {
        view_with(plugins, PluginLoadReport::default())
    }

    fn view_with(plugins: Vec<PluginInfo>, load: PluginLoadReport) -> LiveView {
        let runtime = HostRuntime::new(Context::new()).expect("运行时应该建得起来");
        LiveView::new(runtime, plugins, load, None, TdState::new())
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
        assert_eq!(
            first["abi_version"],
            json!(fuyubot_loader::abi::ABI_VERSION)
        );
        // 没有内核报告时**不编造状态**，如实说 unknown。
        assert_eq!(first["state"], json!("unknown"));
        assert_eq!(first["inject"], json!(["fuyubot.host.td"]));
        assert_eq!(first["file"], json!("libdemo.so"));
        // 老插件没有展示元数据：字段在场但是空串，展示方据此回退到 name。
        assert_eq!(first["display_name"], json!(""));
        assert_eq!(first["description"], json!(""));
        assert!(first.get("reason").is_none(), "没失败就不该有 reason");
    }

    /// 展示名与介绍要原样穿过 `PluginInfo` → JSON（`/plugins` 列表的 `名称（id）— 介绍`）。
    #[test]
    fn plugin_json_carries_display_name_and_description() {
        let mut demo = info("fuyubot.demo", "0.2.0");
        demo.display_name = "示例插件".to_owned();
        demo.description = "最小的插件示例".to_owned();
        let view = view(vec![demo]);

        let parsed: Value = serde_json::from_str(&view.plugins_json()).unwrap();
        assert_eq!(parsed["plugins"][0]["display_name"], json!("示例插件"));
        assert_eq!(parsed["plugins"][0]["description"], json!("最小的插件示例"));
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
        assert_eq!(
            parsed,
            json!({
                "count": 0,
                "scanned": 0,
                "active": 0,
                "pending": 0,
                "activating": 0,
                "failed": 0,
                "unknown": 0,
                "skipped": 0,
                "plugins": [],
                "skipped_plugins": [],
            })
        );
        // 空表与「正常返回」同一份形状：客户端不用为「还没起来」写第二套解析。
        assert_eq!(
            parsed,
            serde_json::from_str::<Value>(&empty_plugins_json()).unwrap()
        );
        assert!(view.is_empty());
    }

    /// 计数是给宿主「一眼看出装了几个、挂了几个」用的：状态计数之和必须等于 `count`，
    /// 跳过的候选单独一组，且带着原因。
    #[test]
    fn list_json_counts_states_and_keeps_skipped_candidates() {
        let mut load = PluginLoadReport {
            scanned: 3,
            ..PluginLoadReport::default()
        };
        load.skip(
            PathBuf::from("/data/user/0/com.example.bot/files/plugins/broken.so"),
            "不是 fuyubot 插件：缺少符号".to_owned(),
        );
        let view = view_with(
            vec![
                info("fuyubot.demo", "0.2.0"),
                info("fuyubot.start", "0.2.0"),
            ],
            load,
        );
        let parsed: Value = serde_json::from_str(&view.plugins_json()).unwrap();

        assert_eq!(parsed["count"], json!(2));
        assert_eq!(parsed["scanned"], json!(3));
        assert_eq!(parsed["skipped"], json!(1));
        // 裸运行时没有报告来源：两个都如实算 unknown，五个计数之和 == count。
        assert_eq!(parsed["unknown"], json!(2));
        assert_eq!(
            ["active", "pending", "activating", "failed", "unknown"]
                .iter()
                .map(|key| parsed[*key].as_u64().unwrap_or_default())
                .sum::<u64>(),
            2
        );
        assert_eq!(
            parsed["skipped_plugins"][0]["path"],
            json!("/data/user/0/com.example.bot/files/plugins/broken.so")
        );
        assert!(
            parsed["skipped_plugins"][0]["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty())
        );
        assert_eq!(view.load_report().scanned, 3);
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
