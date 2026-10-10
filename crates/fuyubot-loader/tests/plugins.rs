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

fn build_fixture(package: &str) -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--offline", "-p", package])
        .current_dir(workspace_root())
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

    let error = loader
        .load_with_config(&path, r#"{"panic_on_start":true}"#)
        .unwrap_err();
    assert!(
        matches!(error, LoadError::StartFailed { code: -1, .. }),
        "期望 StartFailed(-1)，得到 {error}"
    );

    // 失败的那次加载不能留下任何注册
    assert!(loader.runtime().tools().is_empty(), "失败的加载回滚不干净");
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

/// 验收：版本不兼容时 `load` 返回明确错误，不 panic。
#[test]
fn refuses_a_plugin_with_an_incompatible_abi() {
    let path = build_fixture("fuyubot-plugin-badabi");
    let loader = PluginLoader::new(Context::new()).unwrap();

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
    assert!(loader.runtime().tools().is_empty());
    assert!(loader.runtime().events().subscriber_count("demo.event") == 0);
}

/// 不是插件的东西：要么打不开，要么缺 `plugin_init`，两种都不 panic。
#[test]
fn rejects_things_that_are_not_plugins() {
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
