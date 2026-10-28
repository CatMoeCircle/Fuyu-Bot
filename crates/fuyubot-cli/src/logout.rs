//! `fuyubot logout`：安全地清掉**本地** TDLib 会话目录（缺口 22 的独立退出登录）。
//!
//! 删的是 `[telegram].data_dir`（默认 `<项目根>/data`）——TDLib 把 sqlite 库与 binlog
//! 都放在那里，删掉就等于退出登录（**不是** `logOut`：那要连网、还要有效 session，
//! 这里只处理「本机这份会话」）。删完下次 `run` / `login` 会重新要求凭据。
//!
//! # 三道守卫（都写成纯逻辑，单测能离线跑）
//!
//! 1. **实例在跑就拒绝**：判据是 `instance::probe`（那把独占锁），不是「PID 还活着吗」。
//!    运行中删会话目录会把 TDLib 正开着的 sqlite / binlog 拆坏。
//! 2. **默认只预览**：没有 `--yes` 一个字节都不删，只打印「将要删除什么」。
//!    真正删之前再拿一次实例锁，把「预览之后实例刚起来」这个窗口也堵上。
//! 3. **只删看起来真是 TDLib 会话的目录**：必须有一枚 [`SESSION_MARKER`]
//!    （`td.binlog`——TDLib 一开库就写它；本机 `F:\Rust\FuyuBot\data\` 与
//!    `tests` 里的会话目录都核过：`td.binlog` + `db.sqlite` + `db.sqlite-wal/-shm`）。
//!    除此之外的目录（哪怕配置指到它）一律拒绝，只报「看起来不是 TDLib 数据目录」。
//!
//! 另有两条防呆，专门对付「配置里 `data_dir` 写歪了」：
//! - 目录**不在项目根内**：允许删，但额外醒目警告（仍然是「必须 `--yes`」）；
//! - 目录**就是项目根 / 是项目根的上级 / 是文件系统根 / 是符号链接**：直接拒绝。

use std::path::{Path, PathBuf};

use crate::scaffold;
use crate::{existing_root, instance, refuse_while_running, resolved_config_path};

/// TDLib 会话的标记文件。
///
/// TDLib 的 sqlite 库旁边一定躺着一个 `td.binlog`（binlog 是它自己的写前日志）。
/// 拿它当「这里真是会话」的判据，而不是「目录里有没有文件」——用户可能把
/// `data_dir` 指到一个装着别的东西的目录，那种一个字节都不能删。
pub const SESSION_MARKER: &str = "td.binlog";

/// 体检结论：这个 data_dir 现在是什么、能不能删。
#[derive(Debug)]
pub enum Verdict {
    /// 目录不存在：没在这里登录过，或者已经清过了。
    Missing,
    /// 目录存在但是空的：没有会话可删（**不删目录本身**，它可能还会被用到）。
    Empty,
    /// 有东西但没有 TDLib 的会话标记：**拒绝**（多半是配置指错了）。
    NotASession {
        /// 顶层条目名（排序后），报错时打出来让人认领。
        entries: Vec<String>,
    },
    /// 看起来就是 TDLib 的会话目录，可以删。
    Session(Session),
}

/// 一个即将被删除的会话目录（体检结果 + 打印用的统计）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub dir: PathBuf,
    /// 递归数出来的文件数。
    pub files: usize,
    /// 递归数出来的总字节数。
    pub bytes: u64,
    /// 顶层条目名（排序）。只用于打印，不参与删除。
    pub entries: Vec<String>,
    /// 目录是不是在项目根内（不在就额外警告）。
    pub inside_root: bool,
}

/// 体检 + 防呆守卫（**不删任何东西**）。
///
/// `Err` = 这个路径不该碰（符号链接 / 项目根 / 文件系统根 / 项目根的上级）；
/// `Ok(Verdict::NotASession)` = 能碰但不像是 TDLib 会话，由调用方拒绝。
pub fn inspect(root: &Path, data_dir: &Path) -> Result<Verdict, String> {
    let metadata = match std::fs::symlink_metadata(data_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Verdict::Missing),
        Err(error) => return Err(format!("看不了 {}: {error}", data_dir.display())),
    };
    // 符号链接 / 联接：**不跟**。删链接本身是删链接、删目标才是灾难，两种情况都很难
    // 在终端里解释清楚，干脆让人自己确认。
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "{} 是符号链接 / 联接：不跟。确认它指向哪里之后，请手工删目标目录",
            data_dir.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "{} 不是目录（[telegram].data_dir 指错了？）",
            data_dir.display()
        ));
    }

    // 比路径用**规范化后**的绝对路径：`data/../data`、大小写、`\\?\` 前缀都不该让
    // 「在不在项目根内」判错。canonicalize 也可能失败（极端权限），那就退回原样比较。
    let root_abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let dir_abs = data_dir
        .canonicalize()
        .map_err(|error| format!("看不了 {}: {error}", data_dir.display()))?;

    // 这三个是「配置写错一个 `..` 就会到」的地方，直接拒绝，不留任何 --yes 的后门。
    if dir_abs == root_abs {
        return Err(format!(
            "{} 就是项目根本身（[telegram].data_dir 写成了根？）——拒绝删除项目根",
            dir_abs.display()
        ));
    }
    if root_abs.starts_with(&dir_abs) {
        return Err(format!(
            "{} 是项目根的上级目录（[telegram].data_dir 写成了 `..`？）——删它会把项目一起删掉，拒绝",
            dir_abs.display()
        ));
    }
    if dir_abs.parent().is_none() {
        return Err(format!("{} 是文件系统根——拒绝删除", dir_abs.display()));
    }

    let entries = read_names(data_dir)?;
    if entries.is_empty() {
        return Ok(Verdict::Empty);
    }
    if !data_dir.join(SESSION_MARKER).is_file() {
        return Ok(Verdict::NotASession { entries });
    }
    let (files, bytes) = measure(data_dir)?;
    Ok(Verdict::Session(Session {
        dir: data_dir.to_path_buf(),
        files,
        bytes,
        entries,
        inside_root: dir_abs.starts_with(&root_abs),
    }))
}

/// 目录里的顶层条目名（排序）。读不了就是错误——「不知道里面有什么」不能当成「空的」。
fn read_names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|error| format!("读不了 {}: {error}", dir.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| format!("读不了 {} 里的条目: {error}", dir.display()))
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    Ok(names)
}

/// 递归数文件数与总字节数。**不跟符号链接**（`remove_dir_all` 也不跟）。
fn measure(dir: &Path) -> Result<(usize, u64), String> {
    let mut files = 0;
    let mut bytes = 0_u64;
    for entry in
        std::fs::read_dir(dir).map_err(|error| format!("读不了 {}: {error}", dir.display()))?
    {
        let entry = entry.map_err(|error| format!("读不了 {} 里的条目: {error}", dir.display()))?;
        let kind = entry
            .file_type()
            .map_err(|error| format!("看不了 {} 的类型: {error}", entry.path().display()))?;
        if kind.is_dir() {
            let (nested_files, nested_bytes) = measure(&entry.path())?;
            files += nested_files;
            bytes += nested_bytes;
        } else {
            files += 1;
            // 符号链接取 len 会指到链接本身（很短），不计入更诚实。
            if kind.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    bytes += metadata.len();
                }
            }
        }
    }
    Ok((files, bytes))
}

/// 人类可读的大小（只用于打印）。
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 条目名压成一行（太多就截断，别让预览把终端刷掉）。
fn summarize_entries(entries: &[String]) -> String {
    const SHOWN: usize = 12;
    let shown = entries
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join("、");
    if entries.len() > SHOWN {
        format!("{shown}…（还有 {} 项）", entries.len() - SHOWN)
    } else {
        shown
    }
}

/// 打印「将要删除什么」。**预览与真删前都打**——真删时它就是回执。
fn print_plan(session: &Session, root: &Path) {
    println!("将要删除 TDLib 本地会话（logout）：");
    println!("  目录  {}", session.dir.display());
    println!(
        "  内容  {} 个文件，{}",
        session.files,
        human_bytes(session.bytes)
    );
    println!("  条目  {}", summarize_entries(&session.entries));
    if !session.inside_root {
        println!();
        println!(
            "⚠️ 这个目录**不在项目根内**（{}）：[telegram].data_dir 指到了项目外面，",
            root.display()
        );
        println!(
            "   删的是上面那个绝对路径。确认它真是 TDLib 的会话目录（有 {}）再加 --yes。",
            SESSION_MARKER
        );
    }
    println!();
    println!("删掉之后 session 就没了，下次 `fuyubot run` / `fuyubot login` 要重新登录");
    println!("（Bot Token / 手机号 / 密码从来不落盘，这里只有 TDLib 的数据库与 binlog）");
}

/// `fuyubot logout` 的本体。
pub fn run(args: &super::LogoutArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = existing_root(&args.config)?;
    // 守卫 1：实例在跑就拒绝（`--yes` 也一样；拿锁之前先给一句人话）。
    refuse_while_running(&root)?;

    let config_path = resolved_config_path(&args.config, &root);
    let config = scaffold::load_config(&config_path)?;
    let data_dir = config.telegram.data_dir(&root);

    match inspect(&root, &data_dir)? {
        Verdict::Missing => {
            println!(
                "没有会话目录（{} 不存在）：没在这里登录过，或者已经清过了",
                data_dir.display()
            );
            println!("没有东西可删。");
        }
        Verdict::Empty => {
            println!(
                "{} 是空目录（没有 TDLib 的会话标记 {}）：没有会话可删",
                data_dir.display(),
                SESSION_MARKER
            );
        }
        Verdict::NotASession { entries } => {
            return Err(format!(
                "拒绝删除 {}：里面有 {} 项（{}），但没有 TDLib 的会话标记 {}——\
                 它看起来**不是** TDLib 的数据目录。多半是 [telegram].data_dir 指错了；\
                 确认要删这个目录请自己手工删。",
                data_dir.display(),
                entries.len(),
                summarize_entries(&entries),
                SESSION_MARKER
            )
            .into());
        }
        Verdict::Session(session) => {
            print_plan(&session, &root);
            if !args.yes {
                println!();
                println!(
                    "没有 --yes：上面这些**一个都没删**（这只是预览）。\
                     确认要清掉会话就加 --yes 再跑一次。"
                );
                return Ok(());
            }
            // 守卫 1 再挡一次：预览之后、动手之前，可能刚有人 `fuyubot run`。
            // 拿上锁 = 删除过程中不会有实例插进来（拿不到锁就是它已经在跑了）。
            let _lock = instance::InstanceLock::acquire(&root)?;
            std::fs::remove_dir_all(&session.dir).map_err(|error| {
                format!(
                    "删不掉 {}: {error}——可能还有进程拿着它（先 `fuyubot stop`，\
                     或关掉别的 TDLib 客户端）",
                    session.dir.display()
                )
            })?;
            println!();
            println!(
                "已删除 {}（{} 个文件，{}）——下次 `fuyubot run` / `fuyubot login` 会重新要求登录",
                session.dir.display(),
                session.files,
                human_bytes(session.bytes)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::InstanceLock;
    use crate::scaffold;
    use crate::test_support::temp;

    /// 建一个真项目（`fuyubot.toml` + `plugins/`），返回根目录。
    fn project(name: &str) -> PathBuf {
        let root = temp(name);
        scaffold::init(&root, "bot", 1).unwrap();
        root
    }

    /// 在项目里造一份「像 TDLib 会话」的 `data/`（真库落的文件名）。
    fn session(root: &Path) {
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join(SESSION_MARKER), b"binlog").unwrap();
        std::fs::write(data.join("db.sqlite"), b"sqlite").unwrap();
        std::fs::write(data.join("db.sqlite-wal"), b"wal").unwrap();
    }

    fn args(root: &Path, yes: bool) -> crate::LogoutArgs {
        crate::LogoutArgs {
            config: root.join(scaffold::CONFIG_FILE),
            yes,
        }
    }

    /// 守卫 2：**没有 `--yes` 就只预览、一个字节都不删**；给了才真删。
    #[test]
    fn without_yes_it_only_reports_and_deletes_nothing() {
        let root = project("logout-dry-run");
        session(&root);

        run(&args(&root, false)).unwrap();
        assert!(
            root.join("data").join(SESSION_MARKER).is_file(),
            "没有 --yes 却把会话删了"
        );
        assert!(root.join("data").join("db.sqlite").is_file());

        // 同一份夹具给 --yes：这次必须真的删掉（证明上面没删不是因为别的原因）。
        run(&args(&root, true)).unwrap();
        assert!(!root.join("data").exists(), "--yes 之后目录应该没了");
    }

    /// 守卫 3：**没有 TDLib 的会话标记就不许删**（哪怕 `--yes` 也一样）。
    #[test]
    fn a_directory_without_the_tdlib_marker_is_never_deleted() {
        let root = project("logout-no-marker");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("重要资料.txt"), b"keep me").unwrap();

        let error = run(&args(&root, true)).unwrap_err().to_string();
        assert!(error.contains(SESSION_MARKER), "报错要说明缺什么：{error}");
        assert!(
            data.join("重要资料.txt").is_file(),
            "不是 TDLib 会话的目录一个字节都不许动"
        );

        // 空目录：没事可做，也不删（它可能马上就要被 TDLib 用起来），不报错。
        let empty_root = project("logout-empty");
        std::fs::create_dir_all(empty_root.join("data")).unwrap();
        run(&args(&empty_root, true)).unwrap();
        assert!(empty_root.join("data").is_dir(), "空目录不该被删掉");
    }

    /// 守卫 1：**实例在跑就拒绝**（`--yes` 也一样），收摊之后才允许删。
    #[test]
    fn a_running_instance_blocks_logout_even_with_yes() {
        let root = project("logout-running");
        session(&root);

        let lock = InstanceLock::acquire(&root).unwrap();
        // 发布一份状态：报错里的「在跑」与 PID 都来自这里（`status` 文件是跨进程的唯一渠道）。
        lock.publish(&crate::instance::Info {
            pid: 424242,
            started: crate::instance::unix_now(),
            version: "0.1.0".to_owned(),
            plugins: 0,
            name: "bot".to_owned(),
            detail: crate::instance::PluginDetail::default(),
        })
        .unwrap();

        let error = run(&args(&root, true)).unwrap_err().to_string();
        assert!(error.contains("在跑"), "{error}");
        assert!(error.contains("424242"), "报错要带上在跑的 PID：{error}");
        assert!(error.contains("fuyubot stop"), "要指路：{error}");
        assert!(
            root.join("data").join(SESSION_MARKER).is_file(),
            "实例在跑时绝不能删"
        );

        drop(lock);
        run(&args(&root, true)).unwrap();
        assert!(!root.join("data").exists(), "实例收摊后就该能删了");
    }

    /// 配置把 `data_dir` 指到项目外面：**仍然要 `--yes`**，并且体检要标出来
    /// （调用方据此打醒目警告）。真删也允许，但删的必须是那个外面的目录。
    #[test]
    fn a_data_dir_outside_the_project_is_flagged_and_still_needs_yes() {
        let root = project("logout-outside");
        let outside = temp("logout-outside-data");
        std::fs::write(outside.join(SESSION_MARKER), b"binlog").unwrap();

        // 只写数据目录，别的段用不到；Windows 的 `\` 在 TOML 基本字符串里是转义符，
        // 所以统一换成 `/`（std 在 Windows 上认）。
        let data_dir = outside.display().to_string().replace('\\', "/");
        std::fs::write(
            root.join(scaffold::CONFIG_FILE),
            format!("[telegram]\napi_id = 1\ndata_dir = \"{data_dir}\"\n"),
        )
        .unwrap();

        match inspect(&root, &outside).unwrap() {
            Verdict::Session(session) => assert!(!session.inside_root, "外面的目录要标出来"),
            other => panic!("体检成了 {other:?}"),
        }

        // 没有 --yes：预览、不删。
        run(&args(&root, false)).unwrap();
        assert!(outside.join(SESSION_MARKER).is_file());

        // 有 --yes：删外面的那个。
        run(&args(&root, true)).unwrap();
        assert!(!outside.exists(), "指定的 data_dir 才该被删");
        assert!(root.is_dir(), "项目根绝不能被牵连");
    }

    /// 配置写歪成「项目根本身」或「项目根的上级」：**没有任何 --yes 后门**。
    #[test]
    fn the_project_root_and_its_ancestors_are_refused() {
        let root = project("logout-root-guard");
        let error = inspect(&root, &root).unwrap_err();
        assert!(error.contains("项目根"), "{error}");

        let error = inspect(&root, &root.join("..")).unwrap_err();
        assert!(error.contains("上级"), "{error}");
    }
}
