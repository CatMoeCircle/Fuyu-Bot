//! 脚本插件的**同伴文件**约定：与 cdylib 同目录、同主干。
//!
//! 原生插件（旧形态）是一个动态库 + 一个同名脚本：宿主加载 dll 时把脚本的绝对路径
//! 当配置传给中间插件（`fuyubot_python_host` / `fuyubot_js_host`）。这个「去哪找脚本」
//! 的规则以前写在运行时 CLI 的 `scaffold` 里，`fuyubot-sdk check` 想静态查同一件事时
//! 没法用（那是二进制 crate）。现在它归宿主层，**两个消费者共用一份**
//! （`run` 要加载、`check` 要检查）——见缺口 28。

use std::path::{Path, PathBuf};

/// 脚本插件允许的扩展名。
///
/// 顺序就是同类文件同时存在时的**优先级**：`.py` → `.js` → `.ts`。
pub const SCRIPT_EXTENSIONS: [&str; 3] = ["py", "js", "ts"];

/// 插件自己的脚本（`.py` / `.js` / `.ts`）——和 cdylib 同目录同名的那个。
///
/// cargo 在类 Unix 上会给 cdylib 加 `lib` 前缀，带不带前缀都试。
///
/// `.ts` 也在列：TypeScript 插件走 Node 自带的**类型剥离**直接跑（Node ≥ 23.6），
/// 不需要先 `tsc`（见 `examples/ts-counter`）。Node 太老会由桥报「启动失败」，
/// 不会变成「加载成功但什么都不干」。
pub fn find_script(dll: &Path) -> Option<PathBuf> {
    let dir = dll.parent()?;
    let stem = dll.file_stem()?.to_string_lossy().into_owned();
    let bare = stem.strip_prefix("lib").unwrap_or(&stem).to_owned();
    for candidate in [stem, bare] {
        for extension in SCRIPT_EXTENSIONS {
            let path = dir.join(format!("{candidate}.{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试临时目录：仓库约定放 `target/test-tmp`（不用 `TEMP`，见 pitfalls 坑 73）。
    fn temp(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-loader")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        dir
    }

    fn dll_extension() -> &'static str {
        crate::DLL_EXTENSION
    }

    #[test]
    fn script_is_found_next_to_the_library() {
        let dir = temp("script-next-to-lib");
        let dll = dir.join(format!("libweather.{}", dll_extension()));
        std::fs::write(&dll, b"x").unwrap();
        assert_eq!(find_script(&dll), None);
        std::fs::write(dir.join("weather.py"), b"print()").unwrap();
        assert_eq!(
            find_script(&dll).unwrap().file_name().unwrap(),
            "weather.py",
            "要去掉 cargo 加的 lib 前缀"
        );
    }

    /// `.ts` 也在查找列表里（TypeScript 插件直接靠 Node 剥类型跑）。
    /// 同一个名字同时存在时按 `py` → `js` → `ts` 的顺序取第一个。
    #[test]
    fn script_lookup_also_accepts_typescript() {
        let dir = temp("script-ts");
        let dll = dir.join(format!("counter.{}", dll_extension()));
        std::fs::write(&dll, b"x").unwrap();
        std::fs::write(dir.join("counter.ts"), b"export {}").unwrap();
        assert_eq!(
            find_script(&dll).unwrap().file_name().unwrap(),
            "counter.ts"
        );

        // .js 优先于 .ts（两个都在时不该挑 TS）
        std::fs::write(dir.join("counter.js"), b"// js").unwrap();
        assert_eq!(
            find_script(&dll).unwrap().file_name().unwrap(),
            "counter.js"
        );
    }
}
