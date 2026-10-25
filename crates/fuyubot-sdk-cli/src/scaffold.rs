//! 插件脚手架：`fuyubot-sdk init`。
//!
//! 模板用 `include_str!` 嵌进二进制，所以在任何目录都能跑，不需要用户装一份模板文件。
//! Python / JS 的桥与类型声明直接从 `fuyubot-sdk-script` 的源码里取——**只有一份**，
//! 那边改了这边跟着变（`the_example_plugin_bridges_do_not_drift` 把示例也钉住）。
//!
//! 这一整块原来住在 `fuyubot-cli`（运行时 CLI），按新规格搬到开发 CLI：**脚手架是开发的事**。

use std::path::{Path, PathBuf};

const TEMPLATE_RUST_MANIFEST: &str = include_str!("../templates/rust/Cargo.toml");
const TEMPLATE_RUST_SOURCE: &str = include_str!("../templates/rust/src/lib.rs");
const TEMPLATE_C_SOURCE: &str = include_str!("../templates/c/plugin.c");
const TEMPLATE_CPP_SOURCE: &str = include_str!("../templates/cpp/plugin.cpp");
const TEMPLATE_PY_MANIFEST: &str = include_str!("../templates/python/Cargo.toml");
const TEMPLATE_PY_SOURCE: &str = include_str!("../templates/python/src/lib.rs");
const TEMPLATE_PY_SCRIPT: &str = include_str!("../templates/python/plugin.py");
const TEMPLATE_JS_MANIFEST: &str = include_str!("../templates/js/Cargo.toml");
const TEMPLATE_JS_SOURCE: &str = include_str!("../templates/js/src/lib.rs");
const TEMPLATE_JS_SCRIPT: &str = include_str!("../templates/js/plugin.js");

// Python / JS 的桥与类型声明——与 `fuyubot-sdk-script` 里的**同一份文件**。
const BRIDGE_PY: &str = include_str!("../../fuyubot-sdk-script/python/fuyubot_plugin.py");
const STUB_PY: &str = include_str!("../../fuyubot-sdk-script/python/fuyubot_plugin.pyi");
const BRIDGE_JS: &str = include_str!("../../fuyubot-sdk-script/js/fuyubot_plugin.js");
const STUB_JS: &str = include_str!("../../fuyubot-sdk-script/js/fuyubot_plugin.d.ts");

/// 插件语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    C,
    Cpp,
    Python,
    Js,
}

impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Python => "python",
            Self::Js => "js",
        }
    }
}

/// 插件名只允许这些字符：它会**被拼进源码和 TOML**，放开就是注入。
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "插件名 {name:?} 不合法：只允许字母、数字和 . _ -（它会被写进源码）"
        ))
    }
}

/// 插件名归一成能当**目录名 / crate 名 / 文件名**用的东西（`.` 与 `-` 都换成 `_`）。
///
/// 原来住在 `fuyubot-loader`（宿主层），理由是「两个 CLI 都要用同一条规矩」——那条理由
/// 已经不成立：`fuyubot-cli` 从来没调过它。开发 CLI 为了这 6 行把整套宿主层
/// （event / service / tool / middleware / tdlib）拖进了**插件作者**的依赖图，所以搬回来。
fn plugin_file_name(dir: &Path) -> String {
    let stem = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".to_owned());
    stem.replace(['.', '-'], "_")
}

/// 生成插件脚手架，返回插件目录。
///
/// 落在 `<root>/plugins/<规范化后的名字>`——和运行时项目布局一致，`fuyubot` 直接就能加载。
pub fn new_plugin(root: &Path, name: &str, lang: Lang) -> Result<PathBuf, String> {
    check_name(name)?;
    let file_name = plugin_file_name(Path::new(name));
    let dir = root.join("plugins").join(&file_name);
    if dir.exists() {
        return Err(format!("{} 已经存在了", dir.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("建不了 {}: {e}", dir.display()))?;

    let sdk = sdk_path(sdk_crate_for(lang))?
        .display()
        .to_string()
        // 路径要写进 TOML 的**基本字符串**（`"..."`）：Windows 的反斜杠在那里是转义符，
        // `"F:\Rust"` 会被当成非法转义。cargo 认正斜杠，所以直接换掉。
        .replace('\\', "/");
    let render = |template: &str| {
        template
            .replace("__NAME__", name)
            .replace("__CRATE__", &file_name)
            .replace(
                "__SCRIPT__",
                &format!("{file_name}.{}", script_extension(lang)),
            )
            .replace("__SDK_PATH__", &sdk)
    };

    match lang {
        Lang::Rust => {
            write(&dir.join("Cargo.toml"), &render(TEMPLATE_RUST_MANIFEST))?;
            write(&dir.join("src/lib.rs"), &render(TEMPLATE_RUST_SOURCE))?;
        }
        Lang::C => {
            write(
                &dir.join(format!("{file_name}.c")),
                &render(TEMPLATE_C_SOURCE),
            )?;
            write(&dir.join("fuyubot_plugin.h"), fuyubot_abi::PLUGIN_HEADER)?;
        }
        Lang::Cpp => {
            write(
                &dir.join(format!("{file_name}.cpp")),
                &render(TEMPLATE_CPP_SOURCE),
            )?;
            write(&dir.join("fuyubot_plugin.h"), fuyubot_abi::PLUGIN_HEADER)?;
        }
        Lang::Python => {
            write(&dir.join("Cargo.toml"), &render(TEMPLATE_PY_MANIFEST))?;
            write(&dir.join("src/lib.rs"), &render(TEMPLATE_PY_SOURCE))?;
            write(
                &dir.join(format!("{file_name}.py")),
                &render(TEMPLATE_PY_SCRIPT),
            )?;
            // 桥与类型声明：与 SDK 里那份逐字相同。
            write(&dir.join("fuyubot_plugin.py"), BRIDGE_PY)?;
            write(&dir.join("fuyubot_plugin.pyi"), STUB_PY)?;
        }
        Lang::Js => {
            write(&dir.join("Cargo.toml"), &render(TEMPLATE_JS_MANIFEST))?;
            write(&dir.join("src/lib.rs"), &render(TEMPLATE_JS_SOURCE))?;
            write(
                &dir.join(format!("{file_name}.js")),
                &render(TEMPLATE_JS_SCRIPT),
            )?;
            write(&dir.join("fuyubot_plugin.js"), BRIDGE_JS)?;
            write(&dir.join("fuyubot_plugin.d.ts"), STUB_JS)?;
        }
    }
    Ok(dir)
}

fn script_extension(lang: Lang) -> &'static str {
    match lang {
        Lang::Python => "py",
        Lang::Js => "js",
        _ => "txt",
    }
}

/// 把某门语言的 **ABI 绑定文件**刷到 `dir` 里（`fuyubot-sdk codegen`）。
///
/// 与 [`new_plugin`] 的区别：那个是**建新工程**（还会写脚手架源码与模板），
/// 这个只刷绑定，给**已有工程**用——ABI 变了之后重新落一份就行。
///
/// 每一份都来自**唯一那份权威源**，不存在第二份拷贝（AGENTS 约束 39）：
/// - C / C++ 头 → `fuyubot_abi::PLUGIN_HEADER`（cbindgen 的产物，编进 ABI crate 跟着分发）；
/// - Python / JS 的桥与存根 → `fuyubot-sdk-script` 的源码目录，`include_str!` 直接引。
///
/// **会覆盖**同名文件——「刷新」就是这个命令的本意（C 头 / `.pyi` / `.d.ts` 都是**产物**，
/// 不许签进仓库，所以每次编之前刷一份就行）。
pub fn codegen(dir: &Path, lang: Lang) -> Result<Vec<PathBuf>, String> {
    let files: &[(&str, &str)] = match lang {
        Lang::C | Lang::Cpp => &[("fuyubot_plugin.h", fuyubot_abi::PLUGIN_HEADER)],
        Lang::Python => &[
            ("fuyubot_plugin.py", BRIDGE_PY),
            ("fuyubot_plugin.pyi", STUB_PY),
        ],
        Lang::Js => &[
            ("fuyubot_plugin.js", BRIDGE_JS),
            ("fuyubot_plugin.d.ts", STUB_JS),
        ],
        // Rust 侧**没有**「生成出来的绑定」：类型由 `fuyubot-plugin-api` 直接提供，
        // 不是从 ABI 生成的文件。所以这里是空的，调用方负责解释一句。
        Lang::Rust => &[],
    };

    let mut written = Vec::new();
    for (name, content) in files {
        let path = dir.join(name);
        write(&path, content)?;
        written.push(path);
    }
    Ok(written)
}

/// 这门语言的插件要依赖哪个 SDK crate。
///
/// Rust 插件直接对着 `fuyubot-plugin-api` 写；Python / JS 插件的 cdylib 是
/// `fuyubot-sdk-script` 编出来的（它自己再依赖 plugin-api）。
fn sdk_crate_for(lang: Lang) -> &'static str {
    match lang {
        Lang::Python | Lang::Js => "fuyubot-sdk-script",
        _ => "fuyubot-plugin-api",
    }
}

/// 找到某个 SDK crate 的目录。
///
/// `FUYUBOT_SDK_PATH` 指向**装着 SDK crate 的那个目录**（仓库里就是 `crates/`），
/// 一个环境变量管两个 crate。没设就按开发时的仓库布局找。
///
/// 生成的插件用 `path = "..."` 依赖 SDK，因为这些 crate **还没发到 crates.io**；
/// 发出去之后把模板里的依赖换成版本号即可（见 `docs/plugin-guide.md`）。
pub fn sdk_path(crate_name: &str) -> Result<PathBuf, String> {
    let mut roots = Vec::new();
    if let Ok(path) = std::env::var("FUYUBOT_SDK_PATH") {
        if !path.trim().is_empty() {
            roots.push(PathBuf::from(path));
        }
    }
    if let Some(parent) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() {
        roots.push(parent.to_path_buf());
    }

    for root in &roots {
        let candidate = root.join(crate_name);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "找不到 SDK crate {crate_name}：设一下 FUYUBOT_SDK_PATH 指向放着 SDK 的那个目录\
         （现在试过：{}）",
        roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了 {}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("写不了 {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp;

    #[test]
    fn plugin_names_are_validated() {
        assert!(check_name("demo").is_ok());
        assert!(check_name("fuyubot.weather_2").is_ok());
        assert!(check_name("").is_err());
        // 引号能跑到生成的源码里，必须挡掉。
        assert!(check_name("de\"mo").is_err());
        assert!(check_name("de mo").is_err());
        assert!(check_name("../evil").is_err());
    }

    /// 归一化规则：`.` 与 `-` 都得换成 `_`，否则生成的 crate 名 / 文件名不合规。
    /// （这条从 `fuyubot-loader` 搬过来时一并搬，别跟着函数一起丢了。）
    #[test]
    fn plugin_file_names_are_usable_as_crate_names() {
        assert_eq!(plugin_file_name(Path::new("/x/demo")), "demo");
        assert_eq!(
            plugin_file_name(Path::new("/x/fuyubot.weather")),
            "fuyubot_weather"
        );
        assert_eq!(plugin_file_name(Path::new("/x/my-plugin")), "my_plugin");
    }

    #[test]
    fn new_plugin_renders_every_placeholder() {
        let root = temp("new-plugin");
        let dir = new_plugin(&root, "demo", Lang::Rust).unwrap();
        assert_eq!(dir, root.join("plugins").join("demo"));
        let source = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(source.contains("define_plugin!(MyPlugin, \"demo\")"));
        assert!(!source.contains("__NAME__"), "占位符没被替换完");
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"demo\""));
        assert!(manifest.contains("path = "), "{manifest}");
        // 反斜杠在 TOML 基本字符串里是转义符，写进去 cargo 直接报「非法转义」。
        assert!(!manifest.contains('\\'), "{manifest}");
        assert!(manifest.contains("crate-type = [\"cdylib\"]"));
    }

    #[test]
    fn python_plugins_get_the_bridge_and_the_stub() {
        let root = temp("new-python");
        let dir = new_plugin(&root, "weather", Lang::Python).unwrap();
        let bridge = std::fs::read_to_string(dir.join("fuyubot_plugin.py")).unwrap();
        assert_eq!(bridge, BRIDGE_PY, "拷过去的桥必须与 SDK 里那份逐字相同");
        assert!(dir.join("fuyubot_plugin.pyi").is_file());
        let source = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(source.contains("\"weather.py\""), "{source}");
        // 脚本插件依赖的是 sdk-script（不是 plugin-api）——弄错的话 cargo 直接
        // 报 "no matching package named `fuyubot-sdk-script` found"。
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("fuyubot-sdk-script"), "{manifest}");
    }

    /// 示例插件里那些**生成出来的绑定**是从 SDK 拷过去的（脚本得 import 到它）。
    ///
    /// 拷贝就意味着**可能漂移**，所以两个示例都钉住：
    /// Python 用 `.py` + `.pyi`，TS 用 `.js` + `.d.ts`。SDK 改了、示例忘了同步，这条就红。
    #[test]
    fn the_example_plugin_bridges_do_not_drift() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("仓库根");
        let cases = [
            ("python-weather", "fuyubot_plugin.py", BRIDGE_PY),
            ("python-weather", "fuyubot_plugin.pyi", STUB_PY),
            ("ts-counter", "fuyubot_plugin.js", BRIDGE_JS),
            ("ts-counter", "fuyubot_plugin.d.ts", STUB_JS),
        ];
        for (example, name, expected) in cases {
            let path = root.join("examples").join(example).join(name);
            let copied = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("读不了 {}: {e}", path.display()));
            assert_eq!(
                copied, expected,
                "examples/{example}/{name} 与 SDK 里那份不一致：\
                 重新从 crates/fuyubot-sdk-script/ 拷一份（`fuyubot-sdk codegen` 也行）"
            );
        }
    }

    #[test]
    fn c_plugins_get_the_generated_header() {
        let root = temp("new-c");
        let dir = new_plugin(&root, "ping", Lang::C).unwrap();
        let header = std::fs::read_to_string(dir.join("fuyubot_plugin.h")).unwrap();
        assert_eq!(header, fuyubot_abi::PLUGIN_HEADER);
        assert!(header.contains("FUYUBOT_EXPORT PluginHandle plugin_init"));
    }

    #[test]
    fn every_language_writes_something() {
        for (lang, expected) in [
            (Lang::Rust, "src/lib.rs"),
            (Lang::C, "fuyubot_plugin.h"),
            (Lang::Cpp, "fuyubot_plugin.h"),
            (Lang::Python, "fuyubot_plugin.py"),
            (Lang::Js, "fuyubot_plugin.js"),
        ] {
            let root = temp(&format!("lang-{}", lang.as_str()));
            let dir = new_plugin(&root, "demo", lang).unwrap();
            assert!(
                dir.join(expected).is_file(),
                "{} 少了 {expected}",
                lang.as_str()
            );
        }
    }

    /// `codegen` 落的必须是**权威源那一份**，不是第二份拷贝。
    #[test]
    fn codegen_writes_the_authoritative_bindings() {
        let dir = temp("codegen");

        // C / C++：同一份 cbindgen 头
        for lang in [Lang::C, Lang::Cpp] {
            let written = codegen(&dir, lang).unwrap();
            assert_eq!(written.len(), 1, "{lang:?} 应该只写头文件");
            assert_eq!(
                std::fs::read_to_string(dir.join("fuyubot_plugin.h")).unwrap(),
                fuyubot_abi::PLUGIN_HEADER,
                "{lang:?}"
            );
        }

        // Python / JS：桥 + 存根，逐字等于 SDK 源码里那两份
        assert_eq!(codegen(&dir, Lang::Python).unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("fuyubot_plugin.py")).unwrap(),
            BRIDGE_PY
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("fuyubot_plugin.pyi")).unwrap(),
            STUB_PY
        );

        assert_eq!(codegen(&dir, Lang::Js).unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("fuyubot_plugin.js")).unwrap(),
            BRIDGE_JS
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("fuyubot_plugin.d.ts")).unwrap(),
            STUB_JS
        );

        // Rust：没有「生成出来的绑定」，返回空
        assert!(codegen(&dir, Lang::Rust).unwrap().is_empty());
    }

    /// 「刷新」要真的覆盖：写脏再 codegen，内容必须回到权威源。
    #[test]
    fn codegen_overwrites_a_stale_copy() {
        let dir = temp("codegen-overwrite");
        std::fs::write(dir.join("fuyubot_plugin.h"), b"stale").unwrap();
        codegen(&dir, Lang::C).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("fuyubot_plugin.h")).unwrap(),
            fuyubot_abi::PLUGIN_HEADER
        );
    }
}
