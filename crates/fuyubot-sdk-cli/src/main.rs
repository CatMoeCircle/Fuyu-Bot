//! `fuyubot-sdk`：插件**开发** CLI。
//!
//! 与运行时 CLI（`fuyubot`）职责严格分离（见 `docs/ROADMAP.md` 变更 4）：
//!
//! - `fuyubot`（给运维者）：初始化实例、登录、运行、管插件、看状态——**不编译任何东西**。
//! - `fuyubot-sdk`（给插件作者）：脚手架、ABI 绑定、静态检查、开发调试、查 TDLib 方法
//!   ——**也不编译任何东西**。
//!
//! 打包是插件作者用自己的语言工具链完成的事（cargo / gcc / maturin / napi），
//! 两个 CLI 都不参与（变更 5）。
//!
//! 目前实现了三个不需要碰别处的命令：`abi-version` / `td-methods` / `td-method`。
//! 其余命令（`init` / `codegen` / `check` / `attach`）按 `docs/ROADMAP.md` 的 B 逐项补。

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use fuyubot_tdlib::{EntryKind, TdApiSchema, TlEntry};
// `Object` 是 trait（format/architecture/kind/exports 都由它提供），`ObjectKind` 判「是不是动态库」。
use object::{Object, ObjectKind};

mod scaffold;

/// `fuyubot-sdk`：fuyubot 插件开发工具。
#[derive(Parser, Debug)]
#[command(
    name = "fuyubot-sdk",
    version,
    about = "fuyubot 插件开发工具（脚手架 / ABI 绑定 / 检查 / TDLib 方法查询）",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 生成一个插件脚手架
    Init(InitArgs),
    /// 生成 / 刷新某门语言的 ABI 绑定文件（C 头、Python `.pyi`、JS `.d.ts`）
    Codegen(CodegenArgs),
    /// 显示本 SDK 兼容的 ABI 版本
    AbiVersion,
    /// 列出 TDLib 的方法（真 `td_api.tl` 的 `---functions---` 段）
    TdMethods(TdMethodsArgs),
    /// 查看某个 TDLib 方法的签名、参数与说明
    TdMethod(TdMethodArgs),
    /// 静态检查一个插件产物：导出符号与文件类型（**不加载、不运行、不连 Telegram**）
    Check(CheckArgs),
}

#[derive(Args, Debug)]
struct CheckArgs {
    /// 插件产物，例如 plugins/demo/demo.dll（或 `.so` / `.dylib`）
    path: PathBuf,
}

#[derive(Args, Debug)]
struct InitArgs {
    /// 插件名，例如 demo（会写进源码，只允许字母数字和 . _ -）
    name: String,
    /// 插件语言
    #[arg(long, value_enum, default_value_t = LangArg::Rust)]
    lang: LangArg,
    /// 项目目录（默认当前目录）；插件落在它下面的 `plugins/`
    #[arg(long, default_value = ".")]
    dir: PathBuf,
}

#[derive(Args, Debug)]
struct CodegenArgs {
    /// 插件语言
    #[arg(long, value_enum, default_value_t = LangArg::Rust)]
    lang: LangArg,
    /// 写到哪个目录（默认当前目录）；**会覆盖**同名文件
    #[arg(long, default_value = ".")]
    dir: PathBuf,
}

#[derive(Args, Debug)]
struct TdMethodsArgs {
    /// 只看名字里含这段文字的方法（忽略大小写）
    #[arg(long)]
    filter: Option<String>,
}

#[derive(Args, Debug)]
struct TdMethodArgs {
    /// 方法名，例如 sendMessage
    name: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum LangArg {
    Rust,
    C,
    Cpp,
    Python,
    Js,
}

impl From<LangArg> for scaffold::Lang {
    fn from(value: LangArg) -> Self {
        match value {
            LangArg::Rust => Self::Rust,
            LangArg::C => Self::C,
            LangArg::Cpp => Self::Cpp,
            LangArg::Python => Self::Python,
            LangArg::Js => Self::Js,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Init(args) => {
            let lang: scaffold::Lang = args.lang.into();
            let dir = scaffold::new_plugin(&args.dir, &args.name, lang)?;
            println!("已生成 {} 插件脚手架：{}", lang.as_str(), dir.display());
            println!("下一步：用你自己的工具链编译它（cargo build --release / gcc -shared / …）");
            println!("        打包不归 fuyubot-sdk 管——见 docs/ROADMAP.md 变更 5");
        }
        Command::Codegen(args) => {
            let lang: scaffold::Lang = args.lang.into();
            let written = scaffold::codegen(&args.dir, lang)?;
            if written.is_empty() {
                // Rust 侧没有「生成出来的绑定」——类型由 fuyubot-plugin-api 直接提供。
                println!(
                    "{} 没有可生成的绑定：Rust 插件直接用 `fuyubot-plugin-api` 的类型，\
                     它们不是从 ABI 生成的。",
                    lang.as_str()
                );
                println!(
                    "（其它语言：--lang c / cpp 落 `fuyubot_plugin.h`，\
                          --lang python 落 `.py`/`.pyi`，--lang js 落 `.js`/`.d.ts`）"
                );
            } else {
                println!("已刷新 {} 的 ABI 绑定：", lang.as_str());
                for path in written {
                    println!("  {}", path.display());
                }
            }
        }
        Command::AbiVersion => println!("{}", fuyubot_abi::ABI_VERSION),
        Command::TdMethods(args) => {
            for name in method_names(args.filter.as_deref()) {
                println!("{name}");
            }
        }
        Command::TdMethod(args) => print!("{}", render_method(find_function(&args.name)?)),
        Command::Check(args) => {
            let report = check(&args.path)?;
            print!("{}", report.render());
            if !report.loadable() {
                // 检查没过：给一个非零退出码，好让脚本/CI 直接用。
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// `check` 要看的四个 ABI 符号，顺序即输出顺序。
const ABI_SYMBOLS: [&[u8]; 4] = [
    fuyubot_abi::SYM_PLUGIN_INIT,
    fuyubot_abi::SYM_PLUGIN_START,
    fuyubot_abi::SYM_PLUGIN_STOP,
    fuyubot_abi::SYM_PLUGIN_FREE_STRING,
];

/// 一个字面量符号的检查结果。
#[derive(Debug)]
struct SymbolLine {
    name: &'static str,
    present: bool,
    /// 缺了宿主会不会拒绝加载（只有 `plugin_init` 是）。
    required: bool,
}

/// 一次静态检查的结论。纯数据，方便断言。
#[derive(Debug)]
struct CheckReport {
    path: PathBuf,
    format: String,
    architecture: String,
    kind: String,
    is_dynamic: bool,
    symbols: Vec<SymbolLine>,
}

impl CheckReport {
    /// 宿主会不会拒绝加载它。
    fn loadable(&self) -> bool {
        self.is_dynamic
            && self
                .symbols
                .iter()
                .all(|line| line.present || !line.required)
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("文件：{}\n", self.path.display()));
        out.push_str(&format!(
            "格式：{} / {} / {}\n",
            self.format, self.architecture, self.kind
        ));
        if !self.is_dynamic {
            out.push_str(
                "  !!  这不是动态库——宿主只加载 cdylib（`.dll` / `.so` / `.dylib`）。\n\
                 \x20     检查 crate-type = [\"cdylib\"]，或链接时加了 `-shared`。\n",
            );
        }
        out.push_str("导出符号：\n");
        for line in &self.symbols {
            let mark = if line.present { "[ok]" } else { "[缺]" };
            let note = if line.required {
                "必需"
            } else if line.present {
                ""
            } else {
                "宿主容错（缺了也能加载）"
            };
            let line_text = format!("  {mark} {:<20} {note}", line.name);
            out.push_str(line_text.trim_end());
            out.push('\n');
        }
        // 版本只能运行时看：它是 plugin_init 的返回值，不是静态符号。
        out.push_str(&format!(
            "ABI 版本：宿主是 {}。插件报的版本**只能运行时判定**——它在 `PluginHandle.abi_version`\n\
             \x20   里，是 `plugin_init` 的返回值，不是静态符号（见 docs/abi-contract.md）。\n",
            fuyubot_abi::ABI_VERSION
        ));
        out.push_str(if self.loadable() {
            "结论：可以加载。\n"
        } else {
            "结论：**宿主会拒绝加载**（缺必需的 plugin_init，或不是动态库）。\n"
        });
        out
    }
}

/// 静态检查一个插件产物。
///
/// **不加载、不运行**（规格要求）：走 `object` 直接解析文件里的导出表，
/// 而不是 dlopen 之后再 dlsym——dlopen 会在宿主进程里跑插件的初始化代码。
///
/// 能看：文件是不是动态库、什么格式/架构、四个契约符号导出了哪些。
/// **看不了**：`PluginHandle.abi_version`（那是返回值，不是符号），以及任何行为。
fn check(path: &Path) -> Result<CheckReport, String> {
    let data = std::fs::read(path).map_err(|e| format!("读不了 {}: {e}", path.display()))?;
    let file = object::File::parse(&*data).map_err(|e| {
        format!(
            "{} 不是能识别的目标文件（PE / ELF / Mach-O）：{e}",
            path.display()
        )
    })?;

    let exports = file
        .exports()
        .map_err(|e| format!("读不了 {} 的导出表：{e}", path.display()))?;
    let exported: Vec<Vec<u8>> = exports
        .filter_map(|export| export.ok())
        // PE 的导出可以是「按序号」的（没有名字），那种我们用不上：插件符号一定按名字导出。
        .filter_map(|export| match export.name() {
            object::NameOrOrdinal::Name(name) => Some(name.to_vec()),
            _ => None,
        })
        .collect();

    let symbols = ABI_SYMBOLS
        .iter()
        .map(|symbol| SymbolLine {
            name: std::str::from_utf8(symbol).expect("符号名都是 ASCII 字面量"),
            present: exported.iter().any(|name| name.as_slice() == *symbol),
            required: *symbol == fuyubot_abi::SYM_PLUGIN_INIT,
        })
        .collect();

    Ok(CheckReport {
        path: path.to_path_buf(),
        format: format!("{:?}", file.format()),
        architecture: format!("{:?}", file.architecture()),
        kind: format!("{:?}", file.kind()),
        is_dynamic: file.kind() == ObjectKind::Dynamic,
        symbols,
    })
}

/// 方法名列表（已排序）。`filter` 是**忽略大小写的子串**匹配。
///
// ponytail: 每次全量扫一遍那 3214 条；1022 个函数扫一遍是微秒级，需要索引时再说。
fn method_names(filter: Option<&str>) -> Vec<&'static str> {
    let filter = filter.map(str::to_lowercase);
    let mut names: Vec<&'static str> = TdApiSchema::compiled()
        .functions()
        .map(|entry| entry.name.as_str())
        .filter(|name| {
            filter
                .as_ref()
                .is_none_or(|needle| name.to_lowercase().contains(needle))
        })
        .collect();
    names.sort_unstable();
    names
}

/// 找一个**可调用**的方法。
///
/// 构造器不能当方法调（`td_invoke` 会失败），所以这里明确拒绝而不是放行。
fn find_function(name: &str) -> Result<&'static TlEntry, String> {
    let entry = TdApiSchema::compiled()
        .get(name)
        .ok_or_else(|| format!("td_api.tl 里没有 {name}"))?;
    match entry.kind {
        EntryKind::Function => Ok(entry),
        EntryKind::Type => Err(format!(
            "{name} 是构造器（`---types---` 段），不能当方法调；要发请求得用 `---functions---` 里的方法"
        )),
        // 真文件里走不到这里（分类逃不掉，见 build.rs 的 panic），留着是为了不静默放行。
        EntryKind::Unknown => Err(format!("{name} 的归类判不出来（td_api.tl 缺分区标记）")),
    }
}

/// 渲染一个方法：`.tl` 风格的签名 + 参数清单 + 说明。
///
/// 输出刻意用 `td_api.tl` 自己的语法（`name p:t p:t = Result`）——TDLib 文档与这个文件
/// 共用的写法，比自造格式好对照。
fn render_method(entry: &TlEntry) -> String {
    let mut out = String::new();
    if let Some(description) = &entry.description {
        out.push_str(description);
        out.push_str("\n\n");
    }
    out.push_str(&signature(entry));
    out.push('\n');
    if !entry.params.is_empty() {
        out.push_str(&format!("\n参数（{}）：\n", entry.params.len()));
        for param in &entry.params {
            out.push_str(&format!("  {}:{}\n", param.name, param.ty));
        }
    }
    out
}

/// `sendMessage chat_id:int53 … = Message`
fn signature(entry: &TlEntry) -> String {
    let mut text = entry.name.clone();
    for param in &entry.params {
        text.push_str(&format!(" {}:{}", param.name, param.ty));
    }
    text.push_str(" = ");
    text.push_str(&entry.result);
    text
}

/// 测试用的小工具（只在 `cargo test` 里编译）。
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// 测试用的临时目录。
    ///
    /// ⚠️ **不许用 `std::env::temp_dir()`**：MSYS（git bash）会把 `TEMP` 改成 `/tmp`，
    /// 这个值在 Windows 上解析成「**当前盘的根**」（`F:\tmp`），在盘根建目录会被拒绝
    /// （`OS error 5 PermissionDenied`）——于是一批测试**换个 shell 就红**（踩过一次）。
    ///
    /// 放 workspace 的 `target/` 下：一定可写、不依赖任何环境变量、也不污染系统临时目录。
    pub fn temp(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(env!("CARGO_PKG_NAME"))
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_definition_is_valid() {
        // clap 的 `debug_assert` 会检查短选项冲突之类的定义错误。
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// 输出格式对着**真 schema** 钉一下：说明在前、签名用 `.tl` 语法、参数逐行带类型。
    #[test]
    fn renders_a_real_method_in_tl_syntax() {
        let entry = find_function("sendMessage").unwrap();
        let text = render_method(entry);

        // 说明（`//@description`）排在最前面——所以签名不是第一行。
        let description = entry
            .description
            .as_deref()
            .expect("真文件里 sendMessage 有说明");
        assert!(text.starts_with(description), "{text}");

        // 签名行：`td_api.tl` 的写法，含 6 个参数与返回类型。
        assert!(
            text.contains("sendMessage chat_id:int53 topic_id:MessageTopic"),
            "{text}"
        );
        assert!(text.contains("= Message"), "{text}");

        // 参数清单：每个参数一行，带类型。
        assert!(text.contains("参数（6）："), "{text}");
        assert!(
            text.contains("  input_message_content:InputMessageContent"),
            "{text}"
        );
    }

    /// 构造器不能当方法调——`updateNewMessage` 是构造器，不是方法。
    #[test]
    fn constructors_are_not_methods() {
        let error = find_function("updateNewMessage").unwrap_err();
        assert!(error.contains("构造器"), "{error}");
        assert!(
            find_function("noSuchMethodAtAll")
                .unwrap_err()
                .contains("没有")
        );
    }

    #[test]
    fn filter_is_a_case_insensitive_substring() {
        let all = method_names(None).len();
        assert!(all > 100, "真文件应该有上千个函数，实际只有 {all}");

        // 子串匹配：`sendMessageAlbum` 的名字里就含 `sendMessage`，所以用它做唯一命中的例子。
        assert_eq!(method_names(Some("sendmessagealbum")), ["sendMessageAlbum"]);
        assert_eq!(
            method_names(Some("SENDMESSAGEALBUM")),
            method_names(Some("sendmessagealbum")),
            "过滤必须忽略大小写"
        );
        assert!(method_names(Some("zzz-nothing")).is_empty());
    }

    /// `check` 要能在一个**真的编出来的 cdylib** 上认出四个符号。
    ///
    /// 自己调 `cargo build` 把 fixture 建出来（和 `fuyubot-loader/tests/plugins.rs` 一个路子），
    /// 这样 `cargo test -p fuyubot-sdk-cli` 单独跑也自洽，不依赖「先跑过全量构建」。
    #[test]
    fn check_recognises_a_real_plugin() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("仓库根");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--offline", "-p", "fuyubot-plugin-demo"])
            .current_dir(root)
            .status()
            .expect("启动 cargo 失败");
        assert!(status.success(), "构建 fixture 失败");

        let name = if cfg!(target_os = "windows") {
            "fuyubot_plugin_demo.dll"
        } else if cfg!(target_os = "macos") {
            "libfuyubot_plugin_demo.dylib"
        } else {
            "libfuyubot_plugin_demo.so"
        };
        let artifact = root.join("target").join("debug").join(name);
        assert!(artifact.is_file(), "没找到产物 {}", artifact.display());

        let report = check(&artifact).unwrap();
        assert!(report.is_dynamic, "kind = {}", report.kind);
        assert!(report.loadable(), "{}", report.render());
        for line in &report.symbols {
            assert!(line.present, "{} 没被认出来", line.name);
        }
        assert!(report.render().contains("可以加载"), "{}", report.render());
    }

    /// 不是插件的东西：能解析的（可执行文件）要报「缺 plugin_init」，
    /// 压根不是目标文件的要直接报错——都不能当成插件放行。
    #[test]
    fn check_refuses_things_that_are_not_plugins() {
        // 当前测试进程自己就是一个可执行文件：能解析，但没有插件符号。
        let me = std::env::current_exe().unwrap();
        let report = check(&me).unwrap();
        let init = report
            .symbols
            .iter()
            .find(|line| line.name == "plugin_init")
            .expect("符号表里一定有 plugin_init 这一行");
        assert!(!init.present, "测试二进制不该导出 plugin_init");
        assert!(init.required, "plugin_init 必须是必需符号");
        assert!(!report.loadable(), "{}", report.render());

        // 内容不是目标文件
        let dir = crate::test_support::temp("check-garbage");
        let garbage = dir.join("nope.dll");
        std::fs::write(&garbage, b"definitely not a library").unwrap();
        let error = check(&garbage).unwrap_err();
        assert!(error.contains("不是能识别的目标文件"), "{error}");
    }
}
