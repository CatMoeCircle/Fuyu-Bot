//! TDLib 文档标记 → Rustdoc Markdown 的转换与 `///` 发射。
//!
//! TDLib 的 `.tl` 注释里带少量 HTML 风格标记（`<b>` / `<code>` / `<a href=…>` / `<pre>`），
//! 原样塞进 `///` 会在 rustdoc 里显示成一坨尖括号。这里**只改标记、不动措辞**：
//! 英文原文照抄，实体与标签换成等价的 Markdown。
//!
//! 由 [`crate::tl_codegen`] 在**生成期**调用（编译期的 `build.rs` 里）；
//! 运行时不做任何转换——`td_api.json` 里存的仍是原始 TL 文本。

/// TDLib 文档标记 → Rustdoc Markdown。
///
/// - `<b>重要</b>` / `<strong>` → `**重要**`
/// - `<i>` / `<em>` → `_…_`
/// - `<code>getChat</code>` → `` `getChat` ``
/// - `<a href="url">文字</a>` → `[文字](url)`
/// - `<br>` / `<p>` → 换行；`<pre>…</pre>` → 围栏代码块
/// - `&lt;` `&gt;` `&amp;` 等实体 → 对应字符
pub fn to_rustdoc(input: &str) -> String {
    rewrite_links(&rewrite_basic_tags(input))
}

/// `<a href="url">文字</a>` → `[文字](url)`；残缺的标签原样保留（宁可难看，不能吃字）。
fn rewrite_links(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<a ") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else {
            out.push_str(after);
            return out;
        };
        let open_tag = &after[..=tag_end];
        let href = attr_value(open_tag, "href").unwrap_or_default();
        let after_open = &after[tag_end + 1..];
        if let Some(close) = after_open.find("</a>") {
            let text = &after_open[..close];
            out.push_str(&format!("[{text}]({href})"));
            rest = &after_open[close + 4..];
        } else {
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// 行内标签 → Markdown；链接单独一轮处理（要保留 href）。
fn rewrite_basic_tags(input: &str) -> String {
    let mut text = input.to_string();
    text = text.replace("<b>", "**").replace("</b>", "**");
    text = text.replace("<strong>", "**").replace("</strong>", "**");
    text = text.replace("<i>", "_").replace("</i>", "_");
    text = text.replace("<em>", "_").replace("</em>", "_");
    text = text.replace("<code>", "`").replace("</code>", "`");
    text = text
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    text = text
        .replace("<pre>", "\n```\n")
        .replace("</pre>", "\n```\n");
    text = text.replace("<p>", "\n\n").replace("</p>", "");
    text = text.replace("&lt;", "<");
    text = text.replace("&gt;", ">");
    text = text.replace("&quot;", "\"");
    text = text.replace("&#39;", "'");
    text = text.replace("&nbsp;", " ");
    text = text.replace("&amp;", "&");
    text
}

/// 从标签里取属性值（`href="…"` 或 `href='…'`）。
fn attr_value(tag: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pattern = format!("{name}={quote}");
        if let Some(pos) = tag.find(&pattern) {
            let rest = &tag[pos + pattern.len()..];
            let end = rest.find(quote)?;
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// 把一段（可能含换行的）文档转成 `indent` 缩进的 `///` 行写进 `out`。
///
/// 空文本什么都不写——生成器靠这个跳过没有文档的条目。
/// 每行去掉行尾空白，免得生成的文件在编辑器里挂着一堆不可见空格。
pub fn emit_doc(out: &mut String, text: &str, indent: &str) {
    let converted = to_rustdoc(text);
    if converted.trim().is_empty() {
        return;
    }
    use std::fmt::Write as _;
    for line in converted.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            let _ = writeln!(out, "{indent}///");
        } else {
            let _ = writeln!(out, "{indent}/// {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_bold_code_and_links() {
        assert_eq!(to_rustdoc("<b>Important</b>"), "**Important**");
        assert_eq!(to_rustdoc("<code>getChat</code>"), "`getChat`");
        assert_eq!(
            to_rustdoc(r#"see <a href="https://example.com">Example</a> here"#),
            "see [Example](https://example.com) here"
        );
    }

    #[test]
    fn converts_entities() {
        assert_eq!(to_rustdoc("a &lt; b &amp;&amp; c"), "a < b && c");
    }

    #[test]
    fn preserves_the_original_wording() {
        let src = "Returns information about a chat.";
        assert_eq!(to_rustdoc(src), src);
        // 方括号不转义：`[文字](url)` 这样的链接要原样活下来。
        assert_eq!(
            to_rustdoc("Uses [brackets] and stuff"),
            "Uses [brackets] and stuff"
        );
    }

    #[test]
    fn pre_becomes_a_fenced_block_and_lines_survive() {
        let converted = to_rustdoc("before <pre>let x = 1;</pre> after");
        assert!(converted.contains("```\nlet x = 1;\n```"), "{converted}");
    }

    #[test]
    fn emit_doc_writes_one_line_per_line_and_skips_empty_text() {
        let mut out = String::new();
        emit_doc(&mut out, "one<br>two", "    ");
        assert_eq!(out, "    /// one\n    /// two\n");

        let mut empty = String::new();
        emit_doc(&mut empty, "   ", "");
        assert_eq!(empty, "");
    }
}
