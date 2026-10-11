//! 守住「发布资产」这条**跨文件契约**。
//!
//! `src/loader/vendored.rs` 里那几个 `ASSET_*` 常量是**原样**拼进下载 URL 的
//! （`release_url()` → `https://github.com/<repo>/releases/download/TDLib-v<版本>/<资产名>`），
//! 而真正产出这些文件的是 `.github/workflows/build-tdlib.yml`（配 `.github/scripts/*.sh`）。
//!
//! 两边一旦漂掉，症状是「自动下载 TDLib 永远 404，或者写出一个坏文件却**不报错**」——
//! 非常难查（踩过一次：CI 从别的项目搬来时 tag、资产名、扩展名、架构名**四处**都不同）。
//! 所以这里直接**读那个 workflow 来比对**：改了一边忘了另一边，这条立刻红。
//!
//! 读不到文件就跳过——把 crate 单独打包出去时不该因为缺 workflow 而红。

use std::path::Path;

/// 读仓库里的一个文本文件（相对 `crates/fuyubot-tdlib`）。
fn repo_text(relative: &str) -> Option<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(path).ok()
}

#[test]
fn the_release_workflow_publishes_exactly_the_asset_names_we_download() {
    let (Some(vendored), Some(workflow)) = (
        repo_text("src/loader/vendored.rs"),
        repo_text("../../.github/workflows/build-tdlib.yml"),
    ) else {
        eprintln!("跳过：读不到 vendored.rs 或 workflow（crate 被单独打包？）");
        return;
    };

    // 从 vendored.rs 里抠出 ASSET_* 的值——用文本扫，免得依赖它们的可见性/feature 门。
    let assets: Vec<&str> = vendored
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("pub const ASSET_")?;
            let (_, value) = rest.split_once("= \"")?;
            value.strip_suffix("\";")
        })
        .collect();
    assert!(
        assets.len() >= 4,
        "没从 vendored.rs 里抠出资产名——常量写法变了？抠到的是 {assets:?}"
    );

    // 去掉所有空白后再比对：容忍排版变化，但名字必须一字不差。
    let compact: String = workflow.split_whitespace().collect();
    assert!(
        assets.len() >= 4 && !compact.is_empty(),
        "workflow 是空的？读到的长度是 {}",
        workflow.len()
    );

    for asset in &assets {
        // ⚠️ 必须钉在 **matrix 的 `asset` 字段**上，而不是「这个名字在文件里出现过」——
        // 踩过一次：旧版 workflow 里这些名字只出现在**发布说明的表格文字**里，
        // 于是「出现过」这条断言在漂移的版本上照样通过，守卫形同虚设。
        assert!(
            compact.contains(&format!("\"asset\":\"{asset}\"")),
            "workflow 的 matrix 里没有 `\"asset\": \"{asset}\"`：\
             资产名必须**精确等于** loader 的 ASSETS 表（它被原样拼进下载 URL）"
        );
    }

    // **反方向也要查**：CI 里出现的资产名必须都在表里。
    // 少了这一半就漏掉「CI 建了、loader 却叫不出名字」的错配——
    // Windows arm64 原来就是这个问题（CI 建它，loader 表里没有 → 资产没人下得到）。
    let mut from_workflow: Vec<String> = Vec::new();
    let mut rest = compact.as_str();
    while let Some(start) = rest.find("\"asset\":\"") {
        let after = &rest[start + "\"asset\":\"".len()..];
        let Some(end) = after.find('"') else { break };
        from_workflow.push(after[..end].to_owned());
        rest = &after[end..];
    }
    from_workflow.sort();
    from_workflow.dedup();
    let mut from_loader: Vec<String> = assets.iter().map(|name| (*name).to_owned()).collect();
    from_loader.sort();
    assert_eq!(
        from_workflow, from_loader,
        "workflow 的 matrix 资产名与 loader 的 ASSETS 表必须**完全一致**（两个方向都要）：\
         左边是 CI 会产出的，右边是 loader 会去找的"
    );

    // 每个平台的**打包 + 校验**两步都要用 `dist/${{ matrix.asset }}` 当路径——
    // 只有真的用它当文件名，上面那条「matrix 里有名字」才等于「发布出来的就是这个名字」。
    // 3 个平台 × (打包 1 + 校验 1) = 6。加了平台就同步
    // `src/loader/vendored.rs` 的 `ASSET_*` 与这里的数字。
    let uses = compact.matches("dist/${{matrix.asset}}").count();
    assert_eq!(
        uses, 6,
        "linux / macos / windows 的打包与校验都应当走 `dist/${{{{ matrix.asset }}}}`（3×2=6），实际找到 {uses} 处"
    );

    // tag 也必须一致：loader 拼的是 `TDLib V<版本>`（中间有空格，URL 里编码成 %20）。
    assert!(
        workflow.contains("TAG=\"TDLib-v${VER}\""),
        "workflow 的 release tag 必须与 loader 的 release_url() 一致（`TDLib-v<版本>`）"
    );

    // Windows **不能**再用 zip：`extract_library` 只认「裸库」和 `.tar.gz`，
    // 收到 zip 会当裸库原样写出——坏文件，而且**不报错**。
    assert!(
        !workflow.contains(".zip"),
        "workflow 里还在产出 .zip——loader 吃不了它（会被当成裸库静默写出）"
    );
}
