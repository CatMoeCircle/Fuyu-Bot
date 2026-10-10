//! `fuyubot build-plugin`：把当前目录的插件编成 cdylib。
//!
//! 两种世界：Rust 系（`Cargo.toml`，Rust / Python / JS 插件都是它）走 cargo，
//! C/C++ 走一个 C 编译器。这里不实现任何宿主函数，只负责「把插件编出来」。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 动态库后缀，与宿主加载插件时认的扩展名**同一个来源**。
pub const DLL_EXTENSION: &str = fuyubot_loader::DLL_EXTENSION;

/// 插件该用哪条编译路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `cargo build`（Rust / Python / JS 插件的 cdylib 都是 Rust 编的）。
    Cargo,
    /// 直接调 C 编译器。
    C,
}

/// 从目录内容猜插件类型：有 `Cargo.toml` 就是 cargo，否则找 `.c` / `.cpp`。
pub fn detect(dir: &Path) -> Result<Kind, String> {
    if dir.join("Cargo.toml").is_file() {
        return Ok(Kind::Cargo);
    }
    if source_files(dir)?.is_empty() {
        return Err(format!(
            "{} 里既没有 Cargo.toml 也没有 .c/.cpp，不像是插件目录（用 --lang 指定？）",
            dir.display()
        ));
    }
    Ok(Kind::C)
}

/// `fuyubot build-plugin` 的入口：编译，返回产物路径。
pub fn build(dir: &Path, release: bool, kind: Option<Kind>) -> Result<PathBuf, String> {
    let kind = match kind {
        Some(kind) => kind,
        None => detect(dir)?,
    };
    match kind {
        Kind::Cargo => build_cargo(dir, release),
        Kind::C => build_c(dir, release),
    }
}

/// cargo 的构建：用 `--message-format=json` 读回真正的产物路径，
/// 而不是去猜 `target/<profile>/` 里哪个文件是刚编出来的。
fn build_cargo(dir: &Path, release: bool) -> Result<PathBuf, String> {
    let mut command = Command::new("cargo");
    command.arg("build").current_dir(dir);
    if release {
        command.arg("--release");
    }
    // 本机装依赖要联网时，宁可让用户显式说「我联得上」，也不要卡在 TLS 上。
    if offline() {
        command.arg("--offline");
    }
    command
        .arg("--message-format=json")
        .stdout(std::process::Stdio::piped());

    let output = command
        .output()
        .map_err(|e| format!("起不了 cargo：{e}（PATH 里有 cargo 吗？）"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 编译日志照旧给人看：只把 JSON 行挑掉。
    for line in stdout.lines() {
        if !line.trim_start().starts_with('{') {
            eprint!("{line}");
        }
    }
    if !output.status.success() {
        return Err(format!(
            "cargo build 失败（{}）。上面是它的输出。",
            output.status
        ));
    }

    let mut artifact = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let is_cdylib = value
            .pointer("/target/crate_types")
            .and_then(|types| types.as_array())
            .is_some_and(|types| types.iter().any(|t| t.as_str() == Some("cdylib")));
        if !is_cdylib {
            continue;
        }
        if let Some(files) = value.get("filenames").and_then(|f| f.as_array()) {
            for file in files.iter().filter_map(|f| f.as_str()) {
                if file.ends_with(DLL_EXTENSION) {
                    artifact = Some(PathBuf::from(file));
                }
            }
        }
    }

    artifact.ok_or_else(|| {
        "cargo 没编出 cdylib——检查 Cargo.toml 里有 [lib] crate-type = [\"cdylib\"]".to_owned()
    })
}

/// C/C++ 的构建：找一个能用的编译器，直接编出一个动态库。
fn build_c(dir: &Path, release: bool) -> Result<PathBuf, String> {
    let sources = source_files(dir)?;
    if sources.is_empty() {
        return Err(format!("{} 里没有 .c/.cpp 源文件", dir.display()));
    }
    // 每次编译前把 cbindgen 生成的头文件刷新一遍：头文件是**产物**，不该被签进仓库，
    // 而插件源码又是 `#include "fuyubot_plugin.h"` 的，所以这里顺手落一份。
    std::fs::write(dir.join("fuyubot_plugin.h"), fuyubot_abi::PLUGIN_HEADER)
        .map_err(|e| format!("写不了 {} 里的 fuyubot_plugin.h: {e}", dir.display()))?;
    let out_dir = dir
        .join("target")
        .join(if release { "release" } else { "debug" });
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("建不了 {}: {e}", out_dir.display()))?;
    let name = fuyubot_loader::plugin_file_name(dir);
    let output = out_dir.join(format!("{name}.{DLL_EXTENSION}"));

    match toolchain()? {
        Toolchain::Gnu(cc) => {
            let mut command = Command::new(&cc);
            command
                .arg("-shared")
                .arg("-fPIC")
                .arg(if release { "-O2" } else { "-O0" })
                .arg("-I")
                .arg(dir)
                .args(&sources)
                .arg("-o")
                .arg(&output);
            run(&mut command, &cc)?;
        }
        Toolchain::Msvc { vcvars } => {
            // cl 需要 vcvars 设好的环境（INCLUDE / LIB），所以套一层 cmd。
            let list = sources
                .iter()
                .map(|source| format!("\"{}\"", source.display()))
                .collect::<Vec<_>>()
                .join(" ");
            let line = format!(
                // `/utf-8` 必须给：源码（含 cbindgen 生成的头文件）是 UTF-8，
                // 而 cl 默认按系统代码页读，中文注释会被拆坏到「注释提前结束、
                // 后面的声明全成语法错误」这种看不出原因的样子。
                "call \"{}\" >nul && cl /nologo /utf-8 /LD /I\"{}\" {list} /Fe:\"{}\"",
                vcvars.display(),
                dir.display(),
                output.display()
            );
            #[cfg(windows)]
            {
                // `raw_arg` 而不是 `arg`：cmd /C 后面的整行必须**原样**递进去，
                // 让 Command 帮忙加一层引号的话，cmd 会把 `call "..."` 解析坏
                // （报 `'"E:\...\vcvars64.bat"' 不是内部或外部命令`）。
                use std::os::windows::process::CommandExt;
                let mut command = Command::new("cmd");
                command.arg("/C").raw_arg(&line);
                run(&mut command, "cl")?;
            }
            #[cfg(not(windows))]
            {
                let _ = line;
                return Err("MSVC 工具链只在 Windows 上可用，这台机器请装 gcc/clang".to_owned());
            }
        }
    }

    if !output.is_file() {
        return Err(format!("编译没报错，但没看到产物 {}", output.display()));
    }
    Ok(output)
}

fn run(command: &mut Command, what: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("起不了 {what}：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} 退出码 {status}"))
    }
}

fn source_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("读不了 {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "c" || extension == "cpp")
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// 本机上有哪个 C 编译器。
enum Toolchain {
    Gnu(String),
    Msvc { vcvars: PathBuf },
}

fn toolchain() -> Result<Toolchain, String> {
    if let Ok(cc) = std::env::var("CC") {
        if !cc.trim().is_empty() {
            return Ok(Toolchain::Gnu(cc));
        }
    }
    for candidate in ["cc", "gcc", "clang"] {
        if which(candidate).is_some() {
            return Ok(Toolchain::Gnu(candidate.to_owned()));
        }
    }
    if let Some(vcvars) = msvc_vcvars() {
        return Ok(Toolchain::Msvc { vcvars });
    }
    Err("找不到 C 编译器：装一个 gcc/clang（或设 CC），\
         或者装 Visual Studio 的 C++ 生成工具。C++ 插件同理。"
        .to_owned())
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Visual Studio 的 `vcvars64.bat`：有它就等于有 cl（cl 自己不在 PATH 上）。
fn msvc_vcvars() -> Option<PathBuf> {
    let root = std::env::var("ProgramFiles(x86)").ok()?;
    let vswhere = Path::new(&root)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vswhere.is_file() {
        return None;
    }
    let output = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .ok()?;
    let install = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if install.is_empty() {
        return None;
    }
    let vcvars = Path::new(&install)
        .join("VC")
        .join("Auxiliary")
        .join("Build")
        .join("vcvars64.bat");
    vcvars.is_file().then_some(vcvars)
}

fn offline() -> bool {
    ["FUYUBOT_OFFLINE", "CARGO_NET_OFFLINE"].iter().any(|name| {
        std::env::var(name).is_ok_and(|value| {
            let value = value.trim();
            !value.is_empty() && value != "0" && value != "false"
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp;

    #[test]
    fn detect_says_cargo_when_there_is_a_manifest() {
        let dir = temp("detect-cargo");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        assert_eq!(detect(&dir).unwrap(), Kind::Cargo);
        std::fs::remove_file(dir.join("Cargo.toml")).unwrap();
        std::fs::write(dir.join("a.c"), "int main(void){return 0;}").unwrap();
        assert_eq!(detect(&dir).unwrap(), Kind::C);
    }
}
