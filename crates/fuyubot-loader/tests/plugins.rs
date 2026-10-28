//! 端到端：真的编一个 cdylib 出来、真的 dlopen 它、真的走一遍 VTable。
//!
//! 测试自己调 `cargo build` 把 fixture 建出来。这样 `cargo test -p fuyubot-loader` 单独跑
//! 也是自洽的，不依赖「先跑过 cargo build --workspace」这种外部前提。

use std::path::{Path, PathBuf};
use std::process::Command;

use fuyubot_core::Context;
use fuyubot_loader::{LoadError, PluginLoader};
use fuyubot_middleware::{Middleware, MiddlewareContext, Next};

fn workspace_root() -> PathBuf {
    // crates/fuyubot-loader → 上两级就是工作区根
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("工作区根")
        .to_path_buf()
}

/// 测试用的临时目录。**故意不用 `%TEMP%` / `tempfile`**——理由见 `AGENTS.md` 的测试约定：
/// MSYS（git bash）把 `TEMP` 改成 `/tmp`，而受限环境里往真实 `%TEMP%` 建目录会被拒
/// （`OS error 5`），一批测试就莫名其妙红了。放 workspace 的 `target/` 下最稳。
///
/// 形状模仿 `tempfile::TempDir`（只提供 `.path()` + drop 清理），所以调用处只改构造那一句。
struct Scratch(PathBuf);

impl Scratch {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // 目录里可能有**已加载的 DLL**，Windows 上删不掉——忽略即可。
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 每次给一个**唯一**目录（进程号 + 自增），所以并行的测试不会互相踩。
fn scratch() -> Scratch {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let dir = workspace_root()
        .join("target/test-tmp")
        .join(env!("CARGO_PKG_NAME"))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
    Scratch(dir)
}

/// cdylib 的产物路径。Windows 是 `名字.dll`，其它平台带 `lib` 前缀。
fn artifact_path(package: &str) -> PathBuf {
    let name = package.replace('-', "_");
    let file = if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    workspace_root().join("target").join("debug").join(file)
}

/// 给嵌套 cargo 一个**可写**的临时目录。
///
/// MSVC 的 `link.exe` 会在 `%TEMP%` 里建 `lnk*.tmp`；受限环境（沙箱 / `%TEMP%` 不可写）下会报
/// `LNK1104: cannot open file '...\Temp\lnk{GUID}.tmp'`，于是**插件根本编不出来**、
/// 一批测试跟着红（见 pitfalls 坑 73）。仓库约定测试的临时目录一律放 `target/test-tmp`
/// （见 AGENTS「测试必须自洽」），这里把同一个约定**传给子进程**。
fn cargo_temp() -> PathBuf {
    let dir = workspace_root()
        .join("target")
        .join("test-tmp")
        .join("cargo-temp");
    std::fs::create_dir_all(&dir).expect("建 cargo 临时目录失败");
    dir
}

fn build_fixture(package: &str) -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--offline", "-p", package])
        .current_dir(workspace_root())
        .env("TEMP", cargo_temp())
        .env("TMP", cargo_temp())
        .status()
        .expect("启动 cargo 失败");
    assert!(status.success(), "构建 fixture {package} 失败");
    let path = artifact_path(package);
    assert!(path.is_file(), "没找到产物 {}", path.display());
    path
}

/// 验收：编一个最小 Rust 插件、加载它、并确认它经 VTable 干的事都落到了宿主上。
#[test]
fn loads_the_demo_plugin_and_its_vtable_calls_land() {
    let path = build_fixture("fuyubot-plugin-demo");
    let loader = PluginLoader::new(Context::new()).unwrap();

    let plugin = loader.load(&path).expect("demo 插件应该加载成功");
    assert_eq!(plugin.name, "fuyubot.demo", "plugin_init 给的名字");
    assert_eq!(plugin.abi_version, fuyubot_abi::ABI_VERSION);
    assert!(!plugin.context().is_null());

    // 这两条只有在插件真的经 VTable 调进宿主之后才可能成立：
    // `register_tool` 与 `on` 都发生在 dylib 内部的 plugin_start 里。
    assert!(
        loader.runtime().tools().contains("demo.echo"),
        "插件没能在宿主上注册工具"
    );
    assert_eq!(
        loader
            .runtime()
            .events()
            .subscriber_count("updateNewMessage"),
        1,
        "插件没能在宿主上订阅 updateNewMessage"
    );
    assert_eq!(
        loader.runtime().events().subscriber_count("response"),
        1,
        "插件没能在宿主上订阅响应"
    );

    // 验收里的「通过 VTable 调用 log」：插件在 init/start 里各打了一条，
    // 宿主这边应该收得到（日志是环形缓冲，宿主自己也能接日志系统）。
    let logs = loader.runtime().log_lines();
    assert!(
        logs.iter().any(|line| line.contains("fuyubot.demo: init")),
        "没收到 init 的日志：{logs:?}"
    );
    assert!(
        logs.iter().any(|line| line.contains("fuyubot.demo: start")),
        "没收到 start 的日志：{logs:?}"
    );

    // drop 即卸载：plugin_stop + 该插件的注册整批回滚 + 动态库卸载。
    loader.unload(plugin);
    assert!(!loader.runtime().tools().contains("demo.echo"));
    assert_eq!(
        loader
            .runtime()
            .events()
            .subscriber_count("updateNewMessage"),
        0
    );
    assert_eq!(loader.runtime().events().subscriber_count("response"), 0);
    assert!(
        loader
            .runtime()
            .log_lines()
            .iter()
            .any(|line| line.contains("fuyubot.demo: stop")),
        "plugin_stop 没被调到"
    );
}

/// ABI v4：探测阶段就要能读到插件自描述，且**不需要 `plugin_start`**。
///
/// 依赖声明靠它——内核在决定要不要激活之前就得知道插件要什么能力。
#[test]
fn probe_reads_the_v4_manifest() {
    let path = build_fixture("fuyubot-plugin-demo");
    let loader = PluginLoader::new(Context::new()).unwrap();

    let probe = loader.probe(&path).expect("探测 demo 插件");
    let manifest = probe
        .manifest
        .expect("ABI v4 的插件应当导出 plugin_manifest");
    assert_eq!(manifest.name, "fuyubot.demo");
    // demo 插件没有依赖：两个列表都该是空的，而不是解析失败的默认值被当成「没声明」。
    assert!(manifest.inject.is_empty(), "{manifest:?}");
    assert!(manifest.soft_inject.is_empty(), "{manifest:?}");
    // 版本是真值，不是 ABI v4 早期那个硬编码的 `"0.0.0"`。
    assert!(!manifest.version.is_empty(), "{manifest:?}");
    assert_ne!(manifest.version, "0.0.0", "{manifest:?}");
}

/// 作用域的端到端：插件在 `start` 里注册私有服务，能不能立刻取回来。
///
/// 这条只能靠真 dlopen 才测得到——`api_provide_service` / `api_get_service` 读的是
/// **线程局部的 owner 栈**，而那个栈由 `plugin_scope()` 在加载路径上压入。
/// 在单元测试里手工叠两层 scope 会踩到栈的边界（查过一轮没定位到根因），
/// 用真插件走真实路径反而干净。
#[test]
fn a_plugin_reads_back_the_service_it_just_provided() {
    let path = build_fixture("fuyubot-plugin-scope-fixture");
    let loader = PluginLoader::new(Context::new()).unwrap();

    let plugin = loader.load(&path).expect("scope 夹具应当加载成功");
    // `start` 里任何一步失败都会让它返回非零，上面那句就 panic 了；
    // 这两条断言是给「失败但宿主没报错」留的双保险。
    let lines = loader.runtime().log_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("拿回了自己注册的那份")),
        "插件没能取回自己刚注册的服务：{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("公共服务仍然可见")),
        "插件看不见根上的公共服务（父链断了）：{lines:?}"
    );
    assert_eq!(plugin.name, "fuyubot.scope-fixture");
}

/// 端到端：`plugin_manifest` 里的依赖与版本真的能穿过 dlopen/JSON/`Manifest` 三层。
///
/// 用独立夹具而不是 demo，是因为**版本号必须与 `fuyubot-abi` 不同**才证明得了
/// `env!("CARGO_PKG_VERSION")` 求值在插件 crate 那一侧（同 workspace 同版本时，
/// 「取错了 crate」和「取对了」的结果一模一样）。
#[test]
fn probe_reads_declared_dependencies_and_the_plugin_crate_version() {
    let path = build_fixture("fuyubot-plugin-manifest-fixture");
    let loader = PluginLoader::new(Context::new()).unwrap();

    let probe = loader.probe(&path).expect("探测 manifest 夹具");
    let manifest = probe.manifest.expect("夹具应当导出 plugin_manifest");

    assert_eq!(manifest.name, "fuyubot.manifest-fixture");
    // 夹具的 Cargo.toml 里写的是 2.3.4，workspace 是 0.1.x。
    assert_eq!(manifest.version, "2.3.4");
    assert_eq!(manifest.inject, ["demo.dep"]);
    assert_eq!(manifest.soft_inject, ["demo.soft"]);
}

/// 中间件：往 payload 里塞一个标记，用来验证「改过的版本」能到插件手里。
struct Tagger;

impl Middleware for Tagger {
    fn name(&self) -> &str {
        "tagger"
    }

    fn handle(&self, ctx: &mut MiddlewareContext, next: Next<'_>) -> fuyubot_core::Result<()> {
        if let Some(object) = ctx.payload.as_object_mut() {
            object.insert("tagged_by_middleware".to_owned(), serde_json::json!(true));
        }
        next.run(ctx)
    }
}

fn logs_contain(loader: &PluginLoader, needle: &str) -> bool {
    loader
        .runtime()
        .log_lines()
        .iter()
        .any(|line| line.contains(needle))
}

/// 验收：模拟一条 updateNewMessage，经过完整管线，插件回调被调用。
#[test]
fn update_reaches_the_plugin_through_the_pipeline() {
    let path = build_fixture("fuyubot-plugin-demo");
    let loader = PluginLoader::new(Context::new()).unwrap();
    let _plugin = loader.load(&path).expect("demo 插件应该加载成功");

    let outcome = loader
        .pipeline()
        .process(r#"{"@type":"updateNewMessage","message":{"id":11}}"#)
        .unwrap();

    assert_eq!(outcome.update_type, "updateNewMessage");
    assert!(outcome.listeners >= 1, "插件监听器应该被调到");
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert!(
        logs_contain(
            &loader,
            r#"updateNewMessage {"@type":"updateNewMessage","message":{"id":11}}"#
        ),
        "插件没有收到这条更新：{:?}",
        loader.runtime().log_lines()
    );
}

/// 验收：中间件修改更新后，插件收到修改后的版本。
#[test]
fn middleware_rewrites_reach_the_plugin() {
    let path = build_fixture("fuyubot-plugin-demo");
    let loader = PluginLoader::new(Context::new()).unwrap();
    let _plugin = loader.load(&path).expect("demo 插件应该加载成功");

    // 挂一层会把 payload 改掉的中间件
    let _tagger = loader
        .runtime()
        .middleware()
        .use_middleware(fuyubot_middleware::Stage::PreRoute, Tagger);

    loader
        .pipeline()
        .process(r#"{"@type":"updateNewMessage","message":{"id":12}}"#)
        .unwrap();

    let logs = loader.runtime().log_lines();
    let received = logs
        .iter()
        .find(|line| line.contains("\"id\":12"))
        .unwrap_or_else(|| panic!("插件没收到这条更新：{logs:?}"));
    assert!(
        received.contains("tagged_by_middleware"),
        "插件收到的是没被改过的版本：{received}"
    );
    // 原始字段一个都没被剥掉
    assert!(received.contains(r#""message":{"id":12}"#), "{received}");
}

/// 验收：插件发起 td_invoke，收到响应，回调被正确触发。
#[test]
fn plugin_receives_its_own_response() {
    let path = build_fixture("fuyubot-plugin-demo");
    let loader = PluginLoader::new(Context::new()).unwrap();
    let _plugin = loader.load(&path).expect("demo 插件应该加载成功");

    // 插件在 start 里发了一条 getMe
    let request = loader
        .runtime()
        .sent_requests()
        .into_iter()
        .find(|request| request.method == "getMe")
        .expect("插件应该在 start 里发 getMe");

    // 模拟 td_receive 把响应交回来
    let delivered = loader
        .runtime()
        .deliver_response(&serde_json::json!({
            "@type": "user",
            "@extra": request.id.to_string(),
            "id": 4242,
        }))
        .expect("投递响应不该失败");
    assert_eq!(delivered, 1, "应该只投给发起请求的那个插件");

    assert!(
        logs_contain(&loader, "\"id\":4242"),
        "插件没有收到自己的响应：{:?}",
        loader.runtime().log_lines()
    );
    assert!(logs_contain(&loader, "fuyubot.demo: response"));
}

/// 插件在 start 里 panic：`define_plugin!` 接住它，`load` 报错，宿主照旧能用。
#[test]
fn a_panicking_plugin_does_not_take_down_the_host() {
    let path = build_fixture("fuyubot-plugin-demo");
    let loader = PluginLoader::new(Context::new()).unwrap();
    // 宿主自己预置了内置工具（TDLib 函数表），所以「干净」= 回到加载前，而不是「空表」。
    let tools_before = loader.runtime().tools().len();

    let error = loader
        .load_with_config(&path, r#"{"panic_on_start":true}"#)
        .unwrap_err();
    assert!(
        matches!(error, LoadError::StartFailed { code: -1, .. }),
        "期望 StartFailed(-1)，得到 {error}"
    );

    // 失败的那次加载不能留下任何注册
    assert_eq!(
        loader.runtime().tools().len(),
        tools_before,
        "失败的加载回滚不干净"
    );
    assert_eq!(
        loader
            .runtime()
            .events()
            .subscriber_count("updateNewMessage"),
        0
    );

    // 宿主还能继续干活：再加载一次正常的
    let plugin = loader.load(&path).expect("第二次加载应该成功");
    assert!(loader.runtime().tools().contains("demo.echo"));
    drop(plugin);
}

/// 验收：版本不兼容时 `load` 返回明确错误，不 panic，**并且把插件侧的状态还回去**。
#[test]
fn refuses_a_plugin_with_an_incompatible_abi() {
    let path = build_fixture("fuyubot-plugin-badabi");

    // 先自己打开一份：loader 的失败路径会把它的引用还掉，模块一旦卸载 static 就重置，
    // 计数器归零就测不到「刚才到底还回去了没有」。
    // SAFETY: 只从同一个 dll 取一个签名固定的导出函数。
    let probe_lib = unsafe { libloading::Library::new(&path) }.expect("再打开一次 badabi");
    let free_calls: libloading::Symbol<extern "C" fn() -> usize> =
        unsafe { probe_lib.get(b"badabi_free_calls") }.expect("badabi 应当导出 badabi_free_calls");

    let loader = PluginLoader::new(Context::new()).unwrap();
    // 宿主内置工具在加载前就在表里；这里要的是「失败没有新增注册」。
    let tools_before = loader.runtime().tools().len();

    // 探测路径同样会调 init，所以同样必须在收手之前归还（版本不匹配也得还）。
    let probed = loader.probe(&path).expect("probe 只读元数据，不校验版本");
    assert_eq!(probed.abi_version, fuyubot_abi::ABI_VERSION + 100);
    assert_eq!(
        free_calls(),
        1,
        "probe 也要调 plugin_free_handle——一次探测不该在插件侧留下状态"
    );

    let error = loader.load(&path).unwrap_err();
    let text = error.to_string();
    match &error {
        LoadError::AbiMismatch { plugin, host, .. } => {
            assert_eq!(*plugin, fuyubot_abi::ABI_VERSION + 100);
            assert_eq!(*host, fuyubot_abi::ABI_VERSION);
        }
        other => panic!("期望 AbiMismatch，得到 {other}"),
    }
    assert!(text.contains("badabi") || text.contains("ABI"), "{text}");

    // 失败之后宿主必须干净：不能留下半个插件注册的东西。
    assert_eq!(loader.runtime().tools().len(), tools_before);
    assert!(loader.runtime().events().subscriber_count("demo.event") == 0);

    // 插件侧也一样：版本对不上时不能调 plugin_stop（那是对不认识的 ABI 下手），
    // 但 v4 的 plugin_free_handle 就是为这一刻留的出口。
    assert_eq!(
        free_calls(),
        2,
        "版本不匹配时宿主应当调 plugin_free_handle 把 init 分配的状态还回来"
    );
}

/// 不是插件的东西：要么打不开，要么缺 `plugin_init`，两种都不 panic。
#[test]
fn rejects_things_that_are_not_plugins() {
    let dir = scratch();
    let loader = PluginLoader::new(Context::new()).unwrap();

    // 内容不是动态库
    let garbage = dir
        .path()
        .join(format!("garbage.{}", fuyubot_loader::DLL_EXTENSION));
    std::fs::write(&garbage, b"definitely not a shared library").unwrap();
    match loader.load(&garbage) {
        Err(LoadError::Library { .. }) => {}
        other => panic!("期望 Library 错误，得到 {other:?}"),
    }

    // 根本不存在的路径
    let missing = dir
        .path()
        .join(format!("nope.{}", fuyubot_loader::DLL_EXTENSION));
    assert!(matches!(
        loader.load(&missing),
        Err(LoadError::Library { .. })
    ));
}

/// 目录扫描：只挑对应平台的动态库，单个失败不影响其它。
#[test]
fn load_dir_skips_non_plugins() {
    let dir = scratch();
    let demo = build_fixture("fuyubot-plugin-demo");
    std::fs::copy(&demo, dir.path().join(demo.file_name().unwrap())).unwrap();
    std::fs::write(dir.path().join("readme.txt"), b"ignored").unwrap();

    let loader = PluginLoader::new(Context::new()).unwrap();
    let results = loader.load_dir(dir.path()).unwrap();
    assert_eq!(results.len(), 1, "只该尝试一个候选文件");
    let (path, outcome) = &results[0];
    assert!(
        outcome.is_ok(),
        "{} 应该加载成功：{outcome:?}",
        path.display()
    );
}

// ── P0：动态库插件接进内核 Fiber 状态机 ────────────────────────────────
//
// 这一组的要点不是「能加载」，而是「依赖语义只有内核那一份」：探测 → 造适配器 →
// `PluginManager::load_all` → 内核自己决定谁起来、谁等着。下面每条都从**内核的
// 视角**断言（`pending()` / `active()` / `is_running()`），不是从加载器的视角。

/// `inject` 声明的依赖来自配置段 `[plugin.<名>].inject`。
///
/// ⚠️ 插件名是**点分**的（`fuyubot.demo` 在 TOML 里就是 `[plugin.fuyubot.demo]`），
/// 宿主按点路径下钻，所以要拼成嵌套对象。写成字面键 `"fuyubot.demo"` 会查不到，
/// 于是 `inject()` 返回空、插件**立刻激活**——症状是「明明声明了依赖却没等」。
fn with_inject(loader: &PluginLoader, name: &str, deps: &[&str]) {
    let mut section = serde_json::json!({ "inject": serde_json::json!(deps) });
    for segment in name.split('.').rev() {
        section = serde_json::json!({ segment: section });
    }
    loader
        .runtime()
        .install_config(None, serde_json::json!({ "plugin": section }));
}

/// 依赖不满足时**停在 Pending 等**，不是失败；依赖到位后自动激活。
#[test]
fn a_dylib_waits_in_pending_until_its_injection_appears() {
    let demo = build_fixture("fuyubot-plugin-demo");
    // ⚠️ loader 必须装在**内核自己的** Context 上。各自 `new()` 一张表的话，
    // 能力注册到这边、内核的 fiber 看那边，`inject` 永远判不齐（踩过）。
    let mut kernel = fuyubot_core::Kernel::new();
    let ctx = kernel.context().clone();
    let loader = PluginLoader::new(ctx.clone()).unwrap();

    // demo 要一项一开始没人提供的能力。
    with_inject(&loader, "fuyubot.demo", &["database"]);

    let adapter = fuyubot_loader::DylibPlugin::probe(&loader, &demo, "").expect("探测成功");
    let shared = std::sync::Arc::new(adapter);
    kernel
        .plugins()
        .load_all(vec![
            std::sync::Arc::clone(&shared) as std::sync::Arc<dyn fuyubot_core::Plugin>
        ])
        .expect("登记不该失败");

    // 依赖没齐：登记了（is_loaded），但没起来。
    assert!(kernel.plugins().is_loaded("fuyubot.demo"));
    assert!(kernel.plugins().active().is_empty(), "缺依赖时不该激活");
    let pending = kernel.plugins().pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1, vec!["database".to_owned()], "该报出缺什么");
    assert!(!shared.is_running(), "插件代码一次都不该跑到 start");

    // 能力到位 → 内核的钩子自己把它拉起来，不需要谁再调一次加载。
    let _capability = ctx.provide_named("database", 1_u8).expect("注册能力");
    assert_eq!(kernel.plugins().active(), vec!["fuyubot.demo".to_owned()]);
    assert!(shared.is_running(), "依赖齐了就该真的 start 过");
}

/// 能力消失 → 依赖方**自动退回 Pending 并回滚注册**；能力回来 → 自动重新激活。
#[test]
fn a_dylib_deactivates_and_reactivates_with_its_dependency() {
    let demo = build_fixture("fuyubot-plugin-demo");
    let mut kernel = fuyubot_core::Kernel::new();
    let ctx = kernel.context().clone();
    let loader = PluginLoader::new(ctx.clone()).unwrap();
    with_inject(&loader, "fuyubot.demo", &["database"]);

    let shared = std::sync::Arc::new(
        fuyubot_loader::DylibPlugin::probe(&loader, &demo, "").expect("探测成功"),
    );
    kernel
        .plugins()
        .load_all(vec![
            std::sync::Arc::clone(&shared) as std::sync::Arc<dyn fuyubot_core::Plugin>
        ])
        .expect("登记不该失败");

    let capability = ctx.provide_named("database", 1_u8).expect("注册能力");
    assert!(shared.is_running());
    // demo 在 start 里注册过一个工具（见它的 plugin_start）。
    assert!(
        loader.runtime().tools().contains("demo.echo"),
        "激活后它的注册该在宿主上"
    );

    // 能力消失：**Effect 回滚**跑起来 → plugin_stop + 注册整批撤销。
    drop(capability);
    assert!(kernel.plugins().active().is_empty(), "依赖没了就该退回去");
    assert!(!shared.is_running(), "plugin_stop 该被调过");
    assert_eq!(kernel.plugins().pending().len(), 1);
    assert!(
        !loader.runtime().tools().contains("demo.echo"),
        "回滚要把它注册的工具也撤掉，否则是残留"
    );

    // 能力回来：同一个适配器重新 apply，不用重新探测、不用重新加载文件。
    let _again = ctx.provide_named("database", 2_u8).expect("再注册一次");
    assert_eq!(kernel.plugins().active(), vec!["fuyubot.demo".to_owned()]);
    assert!(shared.is_running(), "该重新 start 过");
    assert!(
        loader.runtime().tools().contains("demo.echo"),
        "重新激活要把注册再装回去"
    );
}

/// `unload` 是「依赖方先撤」：撤掉被依赖的插件时，依赖它的先退回 Pending。
#[test]
fn unloading_a_provider_retracts_dependents_first() {
    let demo = build_fixture("fuyubot-plugin-demo");
    let mut kernel = fuyubot_core::Kernel::new();
    let ctx = kernel.context().clone();
    let loader = PluginLoader::new(ctx.clone()).unwrap();
    with_inject(&loader, "fuyubot.demo", &["database"]);

    let shared = std::sync::Arc::new(
        fuyubot_loader::DylibPlugin::probe(&loader, &demo, "").expect("探测成功"),
    );
    kernel
        .plugins()
        .load_all(vec![
            std::sync::Arc::clone(&shared) as std::sync::Arc<dyn fuyubot_core::Plugin>
        ])
        .expect("登记不该失败");

    // 让能力由一个**内核插件**提供，这样卸载它才有「撤提供者」的含义。
    struct Db;
    impl fuyubot_core::Plugin for Db {
        fn name(&self) -> &str {
            "db"
        }
        fn apply(&self, ctx: &Context) -> Result<fuyubot_core::Effect, fuyubot_core::KernelError> {
            ctx.provide_named("database", 1_u8)
        }
    }
    kernel.plugins().load(Db).expect("加载提供者");
    assert!(shared.is_running(), "能力齐了，demo 该起来");

    kernel.plugins().unload("db").expect("卸载提供者");
    assert!(
        !shared.is_running(),
        "提供者一走，依赖方要先撤（plugin_stop 必须被调到）"
    );
    assert!(
        !loader.runtime().tools().contains("demo.echo"),
        "依赖方要先撤掉自己的注册"
    );
    assert_eq!(kernel.plugins().pending().len(), 1);
}

/// 宿主把自己那份报告注册成 service，**插件经 ABI 能读到**——
/// 这就是「插件问不到进程里加载了哪些」那个缺口的答案，且没动 ABI。
#[test]
fn plugins_can_read_the_report_through_a_host_service() {
    let demo = build_fixture("fuyubot-plugin-demo");
    let mut kernel = fuyubot_core::Kernel::new();
    let ctx = kernel.context().clone();
    let loader = PluginLoader::new(ctx.clone()).unwrap();

    let shared = std::sync::Arc::new(
        fuyubot_loader::DylibPlugin::probe(&loader, &demo, "").expect("探测成功"),
    );
    kernel
        .plugins()
        .load_all(vec![
            std::sync::Arc::clone(&shared) as std::sync::Arc<dyn fuyubot_core::Plugin>
        ])
        .expect("登记不该失败");

    // 装一次活来源。之后**不再刷新**——这正是旧实现（存算好的快照）挂掉的地方。
    loader.runtime().install_plugin_reporter(kernel.reporter());

    // 走宿主侧的方法调用（和插件经 vtable 调进去完全同一条路）。
    let json = loader
        .runtime()
        .call_service("fuyubot.host.plugins", "report", "{}")
        .expect("宿主该预置了这个 service");
    let value: serde_json::Value = serde_json::from_str(&json).expect("要是合法 JSON");
    assert_eq!(value["count"], 1);
    assert_eq!(value["plugins"][0]["name"], "fuyubot.demo");
    assert_eq!(value["plugins"][0]["state"], "active");

    // 卸载之后报告要跟着变（不然这个 service 只会说谎）——**不需要谁再刷一次**。
    kernel.plugins().unload("fuyubot.demo").expect("卸载");
    let json = loader
        .runtime()
        .call_service("fuyubot.host.plugins", "report", "{}")
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["count"], 0, "卸完了就不该再有它");
}

/// 宿主预置的「插件清单」service 必须真的在服务表里。
///
/// 少了它，插件侧的 `get_service` 返回 null，`/plugins` 会静默回退到目录清单——
/// 也就是这个功能**看起来**还在、只是报的东西不对。那种退化最难察觉，所以单钉一条。
#[test]
fn host_report_service_is_registered() {
    let ctx = Context::new();
    let loader = PluginLoader::new(ctx.clone()).unwrap();
    let names = loader.runtime().service_names();
    assert!(
        names.iter().any(|n| n == "fuyubot.host.plugins"),
        "服务表里该有 fuyubot.host.plugins，实际有 {names:?}"
    );
}

/// 报告是**活的**：插件运行期上下线之后，再问一次就是新状态。
///
/// 这条守的是修掉的那个真 bug——旧实现存的是「算好的快照」，只在加载时刷一次，
/// 于是运行期依赖消失后 `/plugins` 还在报「三个在跑」，而其实退了两个。
/// 那种 bug 不会让任何测试变红，只会让人对着输出做错判断。
#[test]
fn the_report_reflects_runtime_changes_without_anyone_refreshing() {
    let demo = build_fixture("fuyubot-plugin-demo");
    let mut kernel = fuyubot_core::Kernel::new();
    let ctx = kernel.context().clone();
    let loader = PluginLoader::new(ctx.clone()).unwrap();
    with_inject(&loader, "fuyubot.demo", &["database"]);

    let shared = std::sync::Arc::new(
        fuyubot_loader::DylibPlugin::probe(&loader, &demo, "").expect("探测成功"),
    );
    kernel
        .plugins()
        .load_all(vec![
            std::sync::Arc::clone(&shared) as std::sync::Arc<dyn fuyubot_core::Plugin>
        ])
        .expect("登记不该失败");

    // 装一次活来源，之后**再也不碰它**——这正是旧实现挂掉的地方。
    loader.runtime().install_plugin_reporter(kernel.reporter());

    let read = || -> serde_json::Value {
        let json = loader
            .runtime()
            .call_service("fuyubot.host.plugins", "report", "{}")
            .expect("service 该在");
        serde_json::from_str(&json).expect("要是合法 JSON")
    };

    // 此刻：等待依赖。
    let before = read();
    assert_eq!(before["plugins"][0]["state"], "pending", "{before}");

    // 能力到位 → 它自己起来。**没有人去刷新报告。**
    let capability = ctx.provide_named("database", 1_u8).expect("注册能力");
    let after = read();
    assert_eq!(
        after["plugins"][0]["state"], "active",
        "依赖到位后报告该自己变成 active，实际 {after}"
    );

    // 能力消失 → 它自己退回去。
    drop(capability);
    let back = read();
    assert_eq!(
        back["plugins"][0]["state"], "pending",
        "能力没了报告该自己变回 pending，实际 {back}"
    );
}
