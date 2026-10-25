//! 「动态库的兄弟依赖要靠**库自己所在的目录**才找得到」——单独一个测试二进制。
//!
//! ⚠️ **样本得真的带兄弟依赖，负向对照才成立**：CI 产出的 tdjson 把 OpenSSL/zlib
//! 静态链进去了（`VERSION.json` 里 `linkage: static`，import 表只剩系统 DLL），
//! 那种样本上 `Library::new` 本来就该成功。所以下面只在样本目录里带了别的 DLL 时
//! 才断言负向对照，否则明确打印「跳过」——不静默当通过，也不拿静态样本硬造失败。
//!
//! ⚠️ **为什么不能和别的测试放同一个文件**：这条测试有个**负向对照**——
//! 「`libloading::Library::new`（flags = 0）必须加载失败」。它的前提是进程里
//! **还没有**加载过 tdjson 的兄弟 DLL（libssl / libcrypto / z）。
//!
//! 而 `tests/integration.rs` 里别的 `#[ignore]` 测试（`get_me_roundtrip` 等）都会
//! `TdClient::new()`，那已经把这些依赖加载进同一个进程了。Windows 再遇到同名的依赖会
//! **命中已加载的模块**，于是 `Library::new` 也「成功」→ 负向对照的前提失效 → 测试红。
//! libtest 又是**并发**跑同一个进程里的测试，谁先加载完全看调度（踩过一次：加了第 5 条
//! 测试之后这条开始红，因为它按名字排在 `open_library…` 前面）。
//!
//! 所以它必须独占一个进程。跑法：
//!
//! ```powershell
//! $env:TDLIB_TDJSON_PATH = ".tdlib-cache\1.8.68\windows-x86_64"
//! cargo test -p fuyubot-tdlib --test sibling_dependencies --offline -- --ignored
//! ```

use std::path::PathBuf;

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

    // 样本里除了 tdjson 还有别的 DLL 吗？没有就说明它是静态链接的，
    // `Library::new` 成功是**预期**，负向对照无从建立（见文件头）。
    let has_sibling_deps = copied > 1;

    // 负向对照：flags = 0 这条路必须加载失败。
    //
    // 这个断言是「`open_library` 有必要」的**唯一**证据，所以它要求干净进程——
    // 本文件独占一个测试二进制就是为了它（见文件头）。
    if has_sibling_deps {
        assert!(
            unsafe { libloading::Library::new(&target) }.is_err(),
            "`Library::new` 竟然加载成功了——说明依赖不是靠「库自己所在的目录」找到的，\
             这个测试就证明不了 open_library 的必要性。\
             最常见的原因：**同一个进程里已经有别的测试加载过这些依赖**\
             （这条测试必须独占进程跑，见文件头）。"
        );
    } else {
        println!(
            "[loader] 样本没有兄弟 DLL（静态链接的 tdjson），负向对照跳过——只验正向加载。\
             要覆盖约束 30 的场景，得让 TDLIB_TDJSON_PATH 指向一个带 libssl/libcrypto/z 的目录"
        );
    }

    // 正式路径：必须能加载。
    let lib = unsafe { fuyubot_tdlib::loader::open_library(&target) }
        .unwrap_or_else(|error| panic!("open_library 加载 {} 失败：{error}", target.display()));
    drop(lib);
    if has_sibling_deps {
        println!("[loader] open_library 成功加载：兄弟依赖找到了");
    } else {
        println!("[loader] open_library 成功加载（静态样本，无兄弟依赖）");
    }
}
