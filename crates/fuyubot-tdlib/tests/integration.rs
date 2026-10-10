//! 需要真实 TDLib 的集成测试。默认全部 `#[ignore]`。
//!
//! ```powershell
//! $env:TDLIB_TDJSON_PATH = "C:\tdlib\tdjson.dll"
//! cargo test -p fuyubot-tdlib --test integration -- --ignored
//! ```
//!
//! 启用 vendored 时可以直接跑（需要能访问 GitHub Releases）：
//!
//! ```powershell
//! cargo test -p fuyubot-tdlib --features vendored --test integration -- --ignored
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use fuyubot_tdlib::{Routed, TdClient};
use serde_json::{Value, json};

/// 一次 `getMe` 往返：证明库能加载、请求能发出、带 `@extra` 的响应能收回来。
///
/// 没登录的客户端会收到 401 error —— 那也是合法响应，同样算通过。
#[test]
#[ignore = "需要真实 TDLib（TDLIB_TDJSON_PATH 或 vendored）"]
fn get_me_roundtrip() {
    let client = TdClient::new().expect("加载 TDLib 失败");
    let id = client
        .send(serde_json::json!({ "@type": "getMe" }))
        .expect("发送 getMe 失败");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "15s 内没等到 getMe 的响应");
        match client.receive(Duration::from_millis(500)) {
            Some(Routed::Response(response)) => {
                assert_eq!(response.id, id, "响应对上了别的请求");
                assert!(
                    response.payload.get("@type").is_some(),
                    "响应里必须有 @type: {}",
                    response.payload
                );
                // 只断言「收到了响应」是抓不到符号名写错的：把 `td_json_client_send` 写成
                // 多客户端那套 `td_send` 时，TDLib 也会回一条带 `@extra` 的响应，只是内容是
                // `400 Invalid TDLib instance specified`（我们的指针低位被当成了 client_id）。
                if let Some(message) = response.error_message() {
                    assert!(
                        !message.contains("Invalid TDLib instance"),
                        "请求没送到真正的客户端实例上（符号名用错了？）：{message}"
                    );
                }
                return;
            }
            Some(Routed::Update(update)) => {
                println!("忽略更新 {}", update.kind);
            }
            None => {}
        }
    }
}

/// 同步方法走 `td_json_client_execute`：钉住它的**两参数**签名与符号名。
///
/// 老的 `td_execute(request)` 只有一个参数（多客户端那套）。名字写错不会解析失败，
/// 但句柄对不上、arity 也不对——这条用真库直接问一个同步方法，结果一看就知道。
#[test]
#[ignore = "需要真实 TDLib（TDLIB_TDJSON_PATH 或 vendored）"]
fn execute_runs_synchronous_methods() {
    let client = TdClient::new().expect("加载 TDLib 失败");
    let result = client
        .execute(serde_json::json!({ "@type": "getTextEntities", "text": "@test" }))
        .expect("execute 该能直接拿到结果");

    assert_eq!(result["@type"], "textEntities", "{result}");
    assert_eq!(
        result["entities"][0]["type"]["@type"], "textEntityTypeMention",
        "{result}"
    );
}

/// vendored 路径确实被优先级 2 用上了。
#[cfg(feature = "vendored")]
#[test]
#[ignore = "需要启用 vendored 且能下载 Releases 资产"]
fn vendored_library_is_used() {
    let resolved = fuyubot_tdlib::loader::resolve().expect("解析 vendored 动态库失败");
    println!("命中 {}：{}", resolved.source, resolved.target.display());
    assert_eq!(
        resolved.source,
        fuyubot_tdlib::LibrarySource::Vendored,
        "没走 vendored 优先级（TDLIB_TDJSON_PATH 是否被设置了？）"
    );
    assert_eq!(fuyubot_tdlib::TDLIB_VERSION, "1.8.66");
}

// ── 真 Bot 登录（规格变更 3 的验收）──────────────────────────────────────
//
// 凭据**只走环境变量，不落任何文件**（变更 2/3：配置文件不存凭据，
// session 持久化在 `database_dir`）：
//
// ```powershell
// $env:TG_API_ID   = "12345678"
// $env:TG_API_HASH = "..."
// $env:TG_BOT_TOKEN = "123456:AA..."
// $env:TDLIB_TDJSON_PATH = "F:\Rust\FuyuBot\.tdlib-cache\1.8.66\windows-x86_64"
// cargo test -p fuyubot-tdlib --test integration --offline -- --ignored real_bot_login --nocapture
// ```

/// 一次授权流程的结果。
#[derive(Debug)]
struct LoginRun {
    reached_ready: bool,
    /// 这次是否真的发了 `checkAuthenticationBotToken`（没发 = 白的 session 已够用）。
    used_token: bool,
    last_state: String,
}

/// session 目录：放 workspace 的 `target/` 下（沙箱可写），**故意不清理**——
/// 第二次运行就是要靠它证明「session 持久化后不再需要凭据」。
fn session_dirs() -> (PathBuf, PathBuf) {
    let db = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tdlib-login-test");
    let files = db.join("files");
    std::fs::create_dir_all(&files).expect("建 session 目录失败");
    (db, files)
}

fn tdlib_parameters(api_id: i64, api_hash: &str) -> (Value, PathBuf) {
    let (db, files) = session_dirs();
    let params = json!({
        "@type": "setTdlibParameters",
        "use_test_dc": false,
        "database_directory": db.display().to_string(),
        "files_directory": files.display().to_string(),
        "database_encryption_key": "",
        "use_file_database": true,
        "use_chat_info_database": true,
        "use_message_database": true,
        "use_secret_chats": false,
        "api_id": api_id,
        "api_hash": api_hash,
        "system_language_code": "en",
        "device_model": "fuyubot",
        "system_version": "windows",
        "application_version": env!("CARGO_PKG_VERSION"),
    });
    (params, db)
}

/// 驱动 `updateAuthorizationState` 状态机，直到 `ready` / `closed` / 超时。
///
/// 这就是 `fuyubot login` 将来要跑的那段逻辑：TDLib 每次启动都会先报
/// `authorizationStateWaitTdlibParameters`，参数只能在这时报；
/// 之后是 `waitPhoneNumber`（Bot 用 `checkAuthenticationBotToken`，不必等验证码）
/// 或直接 `ready`（session 还在）。
fn drive_authorization(
    client: &TdClient,
    params: &Value,
    token: &str,
    budget: Duration,
) -> LoginRun {
    let deadline = Instant::now() + budget;
    let mut run = LoginRun {
        reached_ready: false,
        used_token: false,
        last_state: "<无>".to_owned(),
    };

    while Instant::now() < deadline {
        let Some(routed) = client.receive(Duration::from_millis(500)) else {
            continue;
        };
        match routed {
            Routed::Response(response) => {
                if let Some(message) = response.error_message() {
                    panic!("TDLib 请求失败：{message}");
                }
            }
            Routed::Update(update) => {
                if update.kind != "updateAuthorizationState" {
                    continue;
                }
                let state = update.payload["authorization_state"]["@type"]
                    .as_str()
                    .unwrap_or("?")
                    .to_owned();
                run.last_state = state.clone();

                match state.as_str() {
                    "authorizationStateWaitTdlibParameters" => {
                        client
                            .send(params.clone())
                            .expect("发 setTdlibParameters 失败");
                    }
                    "authorizationStateWaitPhoneNumber" => {
                        if !run.used_token {
                            client
                                .send(json!({
                                    "@type": "checkAuthenticationBotToken",
                                    "token": token,
                                }))
                                .expect("发 checkAuthenticationBotToken 失败");
                            run.used_token = true;
                        }
                    }
                    "authorizationStateReady" => {
                        run.reached_ready = true;
                        return run;
                    }
                    "authorizationStateClosed" => return run,
                    other => println!("[login] 未处理的授权状态：{other}"),
                }
            }
        }
    }
    run
}

/// 等 `close` 落地（`authorizationStateClosed`），否则第二个客户端会和第一个抢同一个 session 目录。
fn wait_closed(client: &TdClient, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Some(Routed::Update(update)) = client.receive(Duration::from_millis(500)) {
            if update.kind == "updateAuthorizationState"
                && update.payload["authorization_state"]["@type"] == "authorizationStateClosed"
            {
                return;
            }
        }
    }
}

/// 等某个 `@extra` 对应的响应回来。
fn await_response(client: &TdClient, id: fuyubot_tdlib::RequestId, budget: Duration) -> Value {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Some(Routed::Response(response)) = client.receive(Duration::from_millis(500)) {
            if response.id == id {
                if let Some(message) = response.error_message() {
                    panic!("请求失败：{message}");
                }
                return response.payload;
            }
        }
    }
    panic!("等了 {:?} 也没等到响应 {id}", budget);
}

/// 真 Bot 登录：`setTdlibParameters` → `waitPhoneNumber` → `checkAuthenticationBotToken`
/// → `ready` → `getMe` 拿到这个 bot；**然后重启一个客户端**，证明 session 已经落进
/// `database_dir`，第二次不再需要凭据（这正是规格变更 3 的核心主张）。
#[test]
#[ignore = "需要真实 TDLib + 真实 tg api_id/api_hash/bot token（环境变量 TG_API_ID/TG_API_HASH/TG_BOT_TOKEN）"]
fn real_bot_login_reaches_ready_and_reuses_the_session() {
    let api_id: i64 = std::env::var("TG_API_ID")
        .expect("缺环境变量 TG_API_ID")
        .parse()
        .expect("TG_API_ID 不是数字");
    let api_hash = std::env::var("TG_API_HASH").expect("缺环境变量 TG_API_HASH");
    let token = std::env::var("TG_BOT_TOKEN").expect("缺环境变量 TG_BOT_TOKEN");
    let (params, db) = tdlib_parameters(api_id, &api_hash);

    // 第一次：session 目录是空的（如果之前跑过，会直接 ready，下面的 used_token 断言会失败）。
    let first_client = TdClient::new().expect("加载 TDLib 失败");
    let first = drive_authorization(&first_client, &params, &token, Duration::from_secs(90));
    println!("[login] 第一次登录：{first:?}");
    assert!(
        first.reached_ready,
        "90 秒内没到 authorizationStateReady，停在 {}",
        first.last_state
    );
    assert!(
        first.used_token,
        "第一次就没用 token 说明 {} 里有旧 session；删掉它再跑才验得了首次登录",
        db.display()
    );

    let me = await_response(
        &first_client,
        first_client.send(json!({ "@type": "getMe" })).unwrap(),
        Duration::from_secs(20),
    );
    println!("[login] getMe：{me}");
    // 真库 1.8.66 的 `user` **没有 `is_bot` 字段**：bot 判定在 `type.@type`，
    // 用户名也从 `username` 挪到了 `usernames.active_usernames`（按 TDLib 文档写会写错）。
    assert_eq!(
        me["type"]["@type"], "userTypeBot",
        "getMe 拿到的不是 bot：{}",
        me["type"]
    );
    let username = me["usernames"]["active_usernames"][0]
        .as_str()
        .unwrap_or("");
    assert!(!username.is_empty(), "getMe 没给出 username：{me}");

    // session 必须已经写进 database_dir —— 这是「配置文件不存凭据」成立的前提。
    let files: Vec<String> = std::fs::read_dir(&db)
        .expect("读不了 session 目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    println!("[login] {} 里有：{files:?}", db.display());
    assert!(!files.is_empty(), "session 目录是空的，什么都没持久化");

    // 关掉第一个客户端，再拿同一个目录起一个：这次必须**不发 token** 直接 ready。
    first_client.send(json!({ "@type": "close" })).unwrap();
    wait_closed(&first_client, Duration::from_secs(20));
    drop(first_client);

    let second_client = TdClient::new().expect("加载 TDLib 失败");
    let second = drive_authorization(&second_client, &params, &token, Duration::from_secs(60));
    println!("[login] 第二次登录：{second:?}");
    assert!(
        second.reached_ready,
        "第二次没到 ready，停在 {}",
        second.last_state
    );
    assert!(
        !second.used_token,
        "第二次仍然要求 token —— session 没持久化到 database_dir"
    );

    let me_again = await_response(
        &second_client,
        second_client.send(json!({ "@type": "getMe" })).unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(me_again["id"], me["id"], "两次拿到的不是同一个账号");
    println!("[login] session 复用成功：{username}（id {}）", me["id"]);
}

/// 加载动态库时必须让系统**先查这个库自己所在的目录**（AGENTS 约束 30 / 坑 43）。
///
/// 拿真 tdjson 当样本：它 import 了同在目录里的 `libssl-3-x64` / `libcrypto-3-x64` / `z`。
/// 把这一组文件拷到一个不在 PATH 上的临时目录，再分别用两条路加载：
///
/// - `libloading::Library::new`（`flags = 0`）→ **必须失败**：系统只查 exe 目录 + System32 + PATH，
///   报 `LoadLibraryExW { source: 126 }`（ERROR_MOD_NOT_FOUND），而那个错误完全不指向真正的原因
///   （库明明在目录里，缺的是它的依赖）。这条负向断言是让本测试有意义的关键。
/// - `fuyubot_tdlib::loader::open_library` → **必须成功**（绝对路径走 `LOAD_WITH_ALTERED_SEARCH_PATH`）。
///
/// 插件加载器（`fuyubot-loader`）现在也走同一个 helper —— 插件旁边放兄弟 DLL 的部署形态靠它。
#[test]
#[ignore = "需要真实 TDLib（TDLIB_TDJSON_PATH 指向装着 tdjson 及其依赖的目录）"]
fn open_library_finds_the_libraries_sibling_dependencies() {
    let configured = std::env::var("TDLIB_TDJSON_PATH").expect("缺 TDLIB_TDJSON_PATH");
    let configured = PathBuf::from(configured);
    let source_dir = if configured.is_dir() {
        configured
    } else {
        configured
            .parent()
            .expect("TDLIB_TDJSON_PATH 没有父目录")
            .to_owned()
    };

    // 只拷动态库（td_api.tl 之类不参与加载）；拷到临时目录就等于「库在别处、依赖在旁边」。
    //
    // 注意：**不用 `tempfile` / `%TEMP%`**——理由见 `AGENTS.md` 的测试约定（换 shell /
    // 受限环境下 `%TEMP%` 可能建不了目录）。放 workspace 的 `target/` 下最稳。
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp/open-library");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录失败");
    let mut copied = 0;
    for entry in std::fs::read_dir(&source_dir)
        .expect("读不了 TDLIB_TDJSON_PATH")
        .flatten()
    {
        let path = entry.path();
        let is_library = path.extension().is_some_and(|ext| {
            let ext = ext.to_string_lossy();
            ext == "dll" || ext == "so" || ext == "dylib"
        });
        if is_library {
            std::fs::copy(&path, dir.join(path.file_name().unwrap())).expect("拷贝失败");
            copied += 1;
        }
    }
    assert!(copied > 0, "{} 里没有动态库", source_dir.display());

    let target = std::fs::read_dir(&dir)
        .expect("读不了临时目录")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().contains("tdjson"))
        .expect("没拷到 tdjson");
    println!("[loader] 样本 {}", target.display());

    // 负向对照：flags = 0 这条路必须加载失败。
    assert!(
        unsafe { libloading::Library::new(&target) }.is_err(),
        "`Library::new` 竟然加载成功了——说明依赖不是靠「库自己所在的目录」找到的，\
         这个测试就证明不了 open_library 的必要性"
    );

    // 正式路径：必须能加载（否则 tdjson 的兄弟依赖找不到）。
    let lib = unsafe { fuyubot_tdlib::loader::open_library(&target) }
        .expect("open_library 应该靠 LOAD_WITH_ALTERED_SEARCH_PATH 找到兄弟依赖");
    drop(lib);
    println!("[loader] open_library 成功加载：兄弟依赖找到了");
}

/// 构建时提供了 `td_api.tl` 时，检查**编译期烤进二进制**的那份元数据长得对不对。
///
/// 这是唯一能证明「真文件的分类、方法名、参数名确实解析出来了」的方式——内置测试用的是手写样例。
/// 它顺带回答那个悬着的问题：真文件里到底有没有 `---functions---` 分区标记。
///
/// ```powershell
/// $env:TDLIB_TD_API_TL = "C:\path\to\td_api.tl"
/// cargo test -p fuyubot-tdlib --test integration -- --ignored compiled_schema_looks_like_real_td_api
/// ```
#[test]
#[ignore = "需要构建时提供真实的 td_api.tl（TDLIB_TD_API_TL 或 vendored）"]
fn compiled_schema_looks_like_real_td_api() {
    let schema = fuyubot_tdlib::TdApiSchema::compiled();
    println!(
        "来源 {}：{} 条，其中函数 {} 个",
        schema
            .source()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<未知>".to_owned()),
        schema.len(),
        schema.functions().count()
    );

    assert!(
        schema.len() > 300,
        "真文件应该有好几百条条目，实际只有 {}",
        schema.len()
    );

    // 认不出函数 = 文件里没有分区标记，那归类方式就得换，不能靠猜。
    assert!(
        schema.functions().count() > 100,
        "只认出 {} 个函数；td_api.tl 大概没有 ---functions--- 分区标记",
        schema.functions().count()
    );

    for name in [
        "getMe",
        "sendMessage",
        "getChat",
        "downloadFile",
        "answerCallbackQuery",
    ] {
        let entry = schema
            .get(name)
            .unwrap_or_else(|| panic!("td_api.tl 里没有 {name}"));
        assert_eq!(
            entry.kind,
            fuyubot_tdlib::EntryKind::Function,
            "{name} 应该是函数"
        );
    }

    let send = schema.get("sendMessage").unwrap();
    assert!(send.has_param("chat_id"), "sendMessage 应该带 chat_id");
    assert!(
        send.has_param("input_message_content"),
        "sendMessage 应该带 input_message_content"
    );

    // 构造器不能当方法调
    assert!(matches!(
        schema.check_request("updateNewMessage", &serde_json::json!({})),
        Err(fuyubot_tdlib::SchemaError::NotAFunction(_))
    ));
    // 拼错参数名要被抓到
    assert!(matches!(
        schema.check_request(
            "sendMessage",
            &serde_json::json!({ "chat_id": 1, "txet": "x" })
        ),
        Err(fuyubot_tdlib::SchemaError::UnknownParam { .. })
    ));
}
