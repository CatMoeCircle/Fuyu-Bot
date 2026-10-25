//! `td_api.tl` 的解析与 JSON 互转。
//!
//! 这个文件被**两处**包含：
//! - `src/schema.rs`（运行时）：`use crate::tl_schema;`
//! - `build.rs`（编译期）：`#[path = "src/tl_schema.rs"] mod tl_schema;`
//!
//! 共享的理由是**形状只能有一处实现**：编译期把转换结果写成 JSON 烤进二进制，运行时再读回来；
//! 两边要是各写一份 `to_json` / `from_json`，迟早会漂移，而漂移的表现是「编译进去的 schema 读不出来」。
//!
//! 所以这里只用 `std` + `serde_json`（两边都有的依赖），错误一律 `String`——
//! `thiserror` 是运行时门面的事，不该被拖进构建脚本。
//!
//! ## 判定「函数」还是「构造器」
//!
//! 只认文件里的分区标记 `---functions---` / `---types---`。**不能**用「名字首字母大写是否等于
//! 返回类型」这类启发式：`updateNewMessage ... = Update` 是构造器，但首字母大写之后是
//! `UpdateNewMessage` ≠ `Update`，任何这种启发式都会把它误判成函数。
//!
//! 真实 `td_api.tl` 的布局与语法：
//!
//! - **只有 `---functions---` 一个标记**，它**前面的一切都是类型段**（TDLib 不写 `---types---`）。
//!   所以「文件里有函数标记、又没有显式类型标记」时，标记之前的条目按类型段算。
//!   显式写了 `---types---` 却把条目放在它前面，那是真的没标记——保持 [`EntryKind::Unknown`]，不猜。
//! - 开头有一段**内建前奏**：`double ? = Double;` 这种裸 `?` 参数，以及
//!   `vector {t:Type} # [ t ] = Vector t;` 这种显式泛型定义。它们不是 TDLib 的类
//!   （标量已在 `tl_codegen` 里硬编码），归为 [`EntryKind::Unknown`]，见 [`is_builtin_prelude`]。
//!
//! ## 文档注释的四种形态（都要认，少认一种就静默丢文档）
//!
//! - `//@description 主文 [@参数 文档]…` —— 主文；行尾的 `@参数 …` 片段在条目定稿时
//!   **按真参数名**剥进 [`TlEntry::field_docs`]（主文里不再留一截 `@chat_id …` 的尾巴）。
//! - `//-续行` —— 接到主文后面；如果上一条是独立字段行，就接到那个字段的文档后面。
//! - `//@字段名 文档` —— 参数文档本体。
//! - `//@class 类名 @description …` —— 所属类的说明，通常只出现在第一个构造器上，
//!   由 `tl_codegen` 汇到类 enum 的文档里。
//!
//! 真实 `td_api.tl` 里 `//` 注释**只有这四种**（普通 `//` 注释 0 条），其余形态一律
//! 当主文片段兜底，不丢信息。

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// 条目是「构造器」还是「可调用的函数」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// `---types---` 段里的构造器：**不能**当方法调。
    Type,
    /// `---functions---` 段里的函数：这才是 `td_invoke*` 能调的东西。
    Function,
    /// 文件里没有分区标记，判定不了。
    Unknown,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Function => "function",
            Self::Unknown => "unknown",
        }
    }
}

/// 一个参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlParam {
    pub name: String,
    pub ty: String,
}

/// 一条 TL 条目（一个构造器或一个函数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlEntry {
    pub name: String,
    /// `//@description` 主文 + `//-` 续行拼成的一段（**已剥离**行内参数文档）；没有就是 `None`。
    pub description: Option<String>,
    /// 参数文档：`//@字段名 文本` 行与描述行尾部的 `@字段 文本` 片段。
    pub field_docs: BTreeMap<String, String>,
    /// `//@class … @description …`：所属类的说明（通常只在第一个构造器上有）。
    pub class_description: Option<String>,
    pub params: Vec<TlParam>,
    pub result: String,
    pub kind: EntryKind,
}

impl TlEntry {
    pub fn has_param(&self, name: &str) -> bool {
        self.params.iter().any(|param| param.name == name)
    }

    /// 这个参数的文档（生成代码时直接查它）。
    pub fn field_doc(&self, name: &str) -> Option<&str> {
        self.field_docs.get(name).map(String::as_str)
    }
}

/// 条目之前的整块 `//@` / `//-` 注释，在条目定稿时消费掉。
#[derive(Debug, Default)]
struct DocBlock {
    /// 主文与续行，按出现顺序。
    description_lines: Vec<String>,
    /// 独立字段行（`//@字段名 文本`），`//-` 续行会接到最近一个后面。
    field_docs: BTreeMap<String, String>,
    /// 上一条独立字段行的名字（决定 `//-` 接到哪）。
    last_field: Option<String>,
    /// `//@class … @description …`。
    class_description: Option<String>,
}

impl DocBlock {
    /// 喂一行注释（`//` 之后的部分，未 trim 也行）。
    ///
    /// 认得的形态：`-…`（续行）、`@description …`、`@class … @description …`、
    /// `@字段名 …`；`@` 是这些指令的一部分，所以先分支再剥 `@`。
    fn push_comment(&mut self, body: &str) {
        let body = body.trim();
        if body.is_empty() {
            return;
        }

        // `//-续行`：接到上一条独立字段后面，否则接主文。
        if let Some(rest) = body.strip_prefix('-') {
            let rest = rest.trim();
            if let Some(field) = self.last_field.clone() {
                let entry = self.field_docs.entry(field).or_default();
                if !entry.is_empty() {
                    entry.push(' ');
                }
                entry.push_str(rest);
            } else {
                self.description_lines.push(rest.to_owned());
            }
            return;
        }
        // 非续行一律重置「上一条字段」：后续 `//-` 不该再往旧字段上接。
        self.last_field = None;

        if let Some(rest) = body.strip_prefix('@') {
            // `@description 主文…`——要求整词边界，别把 `@descriptionXxx` 也吃进来。
            if let Some(tail) = rest.strip_prefix("description")
                && (tail.is_empty() || tail.starts_with(char::is_whitespace))
            {
                let tail = tail.trim();
                if !tail.is_empty() {
                    self.description_lines.push(tail.to_owned());
                }
                return;
            }

            // `@class 类名 @description …`（类名本身不进产物，只要描述）。
            if let Some(tail) = rest.strip_prefix("class ") {
                if let Some((_, desc)) = tail.trim().split_once("@description") {
                    let desc = desc.trim();
                    if !desc.is_empty() {
                        self.class_description = Some(desc.to_owned());
                    }
                }
                return;
            }

            // `@字段名 文本`：字段形名字（小写/下划线开头）才算文档行。
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let name = &rest[..name_end];
            let tail = &rest[name_end..];
            if is_doc_field_name(name) && (tail.is_empty() || tail.starts_with(char::is_whitespace))
            {
                let text = tail.trim().to_owned();
                self.field_docs.insert(name.to_owned(), text);
                self.last_field = Some(name.to_owned());
                return;
            }
        }

        // 其余形态（真实文件里 0 条）当主文片段兜底，不丢信息。
        self.description_lines.push(body.to_owned());
    }

    /// 条目定稿：逐行剥行内参数文档、拼主文、交出三样东西。
    ///
    /// 剥离**按行进行**：`@参数 文档` 片段到**本行行尾**为止，绝不吃进下一行——
    /// 否则主文行尾的 `@source …` 会把后面的 `//-续行` 一起吞掉。
    fn finish(
        mut self,
        params: &[TlParam],
    ) -> (Option<String>, BTreeMap<String, String>, Option<String>) {
        let param_names: std::collections::BTreeSet<&str> =
            params.iter().map(|param| param.name.as_str()).collect();
        for line in &mut self.description_lines {
            if !line.is_empty() {
                strip_inline_param_docs(line, &param_names, &mut self.field_docs);
            }
        }
        // 拼接并折叠空白（剥除可能留下双空格）。
        let description = self
            .description_lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let description = (!description.is_empty()).then_some(description);
        (description, self.field_docs, self.class_description)
    }
}

/// 是否是「字段文档」的名字形（小写/下划线开头的标识符，且不是保留字）。
///
/// `description` / `class` 要排除：它们是 `//@` 的指令词，不是字段。
/// （TDLib 里叫 description 的参数写作 `param_description`，实测不冲突。）
fn is_doc_field_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name != "description"
        && name != "class"
}

/// 把主文里 `@参数名 文档` 的行内片段剥进 `field_docs`，主文里不再留尾巴。
///
/// **只认真参数名**：`email@example.com` 里的 `example` 不是参数，原样保留——
/// 这就是不在注释行处理时剥离、非要等到定稿（参数已知）才剥离的原因。
fn strip_inline_param_docs(
    description: &mut String,
    param_names: &std::collections::BTreeSet<&str>,
    field_docs: &mut BTreeMap<String, String>,
) {
    let text = std::mem::take(description);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(pos) = rest.find('@') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let name_end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_end];
        if name.is_empty() || !param_names.contains(name) {
            // 不是参数：`@` 原样放回，指针只前进一个字符，继续找。
            out.push('@');
            rest = after;
            continue;
        }
        // 文档文本到下一个 `@`（无论是不是参数）为止。
        let body_start = name_end;
        let body_end = after[body_start..]
            .find('@')
            .map(|i| body_start + i)
            .unwrap_or(after.len());
        let doc = after[body_start..body_end].trim();
        if !doc.is_empty() {
            // 独立字段行已经写过的一律不覆盖（它是作者单独成行的完整版本）。
            field_docs
                .entry(name.to_owned())
                .or_insert_with(|| doc.to_owned());
        }
        rest = &after[body_end..];
    }
    out.push_str(rest);
    *description = out;
}

/// 解析 `td_api.tl` 的全文。错误信息里带行号。
pub fn parse(text: &str) -> Result<Vec<TlEntry>, String> {
    // 带 BOM 的文件（Windows 编辑器存盘常见）不能让第一行注释失效。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // 真实 td_api.tl 只有 `---functions---`，它前面的一切都是类型段（见模块文档）。
    // 显式写过 `---types---` 的文件不适用这条——那时标记之前的条目确实没归属。
    let explicit_types = text.lines().any(|line| is_types_marker(line.trim()));
    let implicit_types =
        !explicit_types && text.lines().any(|line| is_functions_marker(line.trim()));

    let mut entries = Vec::new();
    let mut section: Option<EntryKind> = None;
    let mut docs = DocBlock::default();
    let mut pending = String::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // 分区标记
        if is_functions_marker(line) {
            section = Some(EntryKind::Function);
            continue;
        }
        if is_types_marker(line) {
            section = Some(EntryKind::Type);
            continue;
        }

        // 注释：四种形态都在 DocBlock 里认（见模块文档）。
        if let Some(comment) = line.strip_prefix("//") {
            docs.push_comment(comment);
            continue;
        }

        if !pending.is_empty() {
            pending.push(' ');
        }
        pending.push_str(line);
        if !line.ends_with(';') {
            continue;
        }

        let fallback = if implicit_types {
            EntryKind::Type
        } else {
            EntryKind::Unknown
        };
        let docs = std::mem::take(&mut docs);
        let entry = parse_entry(&pending, docs, section.unwrap_or(fallback))
            .map_err(|e| format!("第 {} 行附近: {e}", index + 1))?;
        entries.push(entry);
        pending.clear();
    }

    if !pending.trim().is_empty() {
        return Err(format!("文件结尾还有没闭合的条目: {pending}"));
    }

    Ok(entries)
}

/// `---functions---` 分区标记。
fn is_functions_marker(line: &str) -> bool {
    line.starts_with("---") && line.contains("function")
}

/// `---types---` 分区标记。
fn is_types_marker(line: &str) -> bool {
    line.starts_with("---") && line.contains("type")
}

/// TL 的内建前奏定义——不是 TDLib 的类，不能生成 Rust 类型。
///
/// - `double ? = Double;` / `string ? = String;`：参数位置只有一个裸 `?`；
/// - `vector {t:Type} # [ t ] = Vector t;`：TL 的显式泛型语法（`{}` / `#` / `[]`），
///   它的结果 `Vector t` 根本不是合法标识符。
///
/// 标量（`int32`/`int53`/`double`/`string`/`bytes`/`bool`）在 `tl_codegen::shape_of` 里已经硬编码，
/// 这里判的是**语法形状**，不是名字——`ensure` 一下：真实构造器不会长这样。
fn is_builtin_prelude(left: &str) -> bool {
    left.contains('#')
        || left.contains('{')
        || left.contains('[')
        || left.split_whitespace().skip(1).any(|token| token == "?")
}

/// `name params = Result;` 里的一行（可能由多行拼起来）+ 它之前的文档块。
fn parse_entry(text: &str, docs: DocBlock, kind: EntryKind) -> Result<TlEntry, String> {
    let text = text.trim().trim_end_matches(';').trim();
    let (left, result) = text
        .split_once('=')
        .ok_or_else(|| format!("缺少 '='：{text}"))?;

    let name = left
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("没有名字：{text}"))?
        .to_owned();
    let result = result.trim().to_owned();

    // 内建前奏：归类成 Unknown（既不当类生成，也不当函数调）。
    if is_builtin_prelude(left) {
        let (description, field_docs, class_description) = docs.finish(&[]);
        return Ok(TlEntry {
            name,
            description,
            field_docs,
            class_description,
            params: Vec::new(),
            result,
            kind: EntryKind::Unknown,
        });
    }

    let mut params = Vec::new();
    for token in left.split_whitespace().skip(1) {
        let (param, ty) = token
            .split_once(':')
            .ok_or_else(|| format!("参数写作 {token}，应该是 name:type"))?;
        params.push(TlParam {
            name: param.to_owned(),
            ty: ty.to_owned(),
        });
    }

    // 文档定稿放在参数已知之后：行内 `@参数 …` 只认真参数名（见 strip_inline_param_docs）。
    let (description, field_docs, class_description) = docs.finish(&params);

    Ok(TlEntry {
        name,
        description,
        field_docs,
        class_description,
        params,
        result,
        kind,
    })
}

/// 条目 → JSON。**编译期与运行时共用这一个形状**，改它要同时想清楚两边。
pub fn to_json(entries: &[TlEntry], source: Option<&str>) -> Value {
    let bucket = |kind: EntryKind| {
        Value::Array(
            entries
                .iter()
                .filter(|entry| entry.kind == kind)
                .map(entry_to_json)
                .collect(),
        )
    };
    json!({
        "source": source,
        "entry_count": entries.len(),
        "functions": bucket(EntryKind::Function),
        "constructors": bucket(EntryKind::Type),
        "unresolved": bucket(EntryKind::Unknown),
    })
}

fn entry_to_json(entry: &TlEntry) -> Value {
    json!({
        "name": entry.name,
        "description": entry.description,
        "field_docs": entry.field_docs,
        "class_description": entry.class_description,
        "params": entry
            .params
            .iter()
            .map(|param| json!({ "name": param.name, "type": param.ty }))
            .collect::<Vec<_>>(),
        "result": entry.result,
        "kind": entry.kind.as_str(),
    })
}

/// JSON → 条目与来源。归属按桶定（`functions` / `constructors` / `unresolved`）。
pub fn from_json(value: &Value) -> Result<(Vec<TlEntry>, Option<String>), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "schema JSON 的顶层必须是对象".to_owned())?;

    let mut entries = Vec::new();
    for (bucket, kind) in [
        ("functions", EntryKind::Function),
        ("constructors", EntryKind::Type),
        ("unresolved", EntryKind::Unknown),
    ] {
        let items = object
            .get(bucket)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for item in items {
            entries.push(entry_from_json(item, kind)?);
        }
    }

    let source = object
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((entries, source))
}

fn entry_from_json(value: &Value, kind: EntryKind) -> Result<TlEntry, String> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "条目缺少 name".to_owned())?
        .to_owned();

    let params = value
        .get("params")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|param| {
            Ok(TlParam {
                name: param
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{name} 的某个参数缺少 name"))?
                    .to_owned(),
                ty: param
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{name} 的某个参数缺少 type"))?
                    .to_owned(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let field_docs = value
        .get("field_docs")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| {
                    Ok((
                        key.clone(),
                        value
                            .as_str()
                            .ok_or_else(|| format!("{name} 的 field_docs.{key} 不是字符串"))?
                            .to_owned(),
                    ))
                })
                .collect::<Result<BTreeMap<String, String>, String>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(TlEntry {
        name,
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        field_docs,
        class_description: value
            .get("class_description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        params,
        result: value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
//@class User @description Represents a user
user id:int53 first_name:string = User;

---types---

//@description A new incoming message
updateNewMessage message:Message = Update;

---functions---

//@description Returns the current user
getMe = User;

//@description Sends a message
sendMessage chat_id:int53 message_thread_id:int53 input_message_content:InputMessageContent reply_markup:ReplyMarkup = Message;
"#;

    fn sample() -> Vec<TlEntry> {
        parse(SAMPLE).unwrap()
    }

    #[test]
    fn splits_by_section_markers() {
        let entries = sample();
        assert_eq!(entries.len(), 4);

        let get_me = entries.iter().find(|e| e.name == "getMe").unwrap();
        assert_eq!(get_me.kind, EntryKind::Function);
        assert!(get_me.params.is_empty());
        assert_eq!(get_me.result, "User");
        assert_eq!(
            get_me.description.as_deref(),
            Some("Returns the current user")
        );

        let send = entries.iter().find(|e| e.name == "sendMessage").unwrap();
        assert_eq!(send.kind, EntryKind::Function);
        assert!(send.has_param("input_message_content"));
        assert!(!send.has_param("content"));

        // 分区标记之前的那条判不了归属
        let user = entries.iter().find(|e| e.name == "user").unwrap();
        assert_eq!(user.kind, EntryKind::Unknown);

        // ---types--- 之后的构造器必须判成 Type
        let update = entries
            .iter()
            .find(|e| e.name == "updateNewMessage")
            .unwrap();
        assert_eq!(update.kind, EntryKind::Type);
    }

    #[test]
    fn the_real_layout_has_only_a_functions_marker_so_the_prefix_is_the_types_section() {
        // 真实 td_api.tl：内建前奏 → 类型段（无标记）→ `---functions---` → 函数段。
        let entries = parse(
            "double ? = Double;\n\
             string ? = String;\n\
             int32 = Int32;\n\
             vector {t:Type} # [ t ] = Vector t;\n\
             user id:int53 first_name:string = User;\n\
             ---functions---\n\
             getMe = User;\n",
        )
        .unwrap();

        // 内建前奏不是 TDLib 的类，也不能把解析卡住
        for name in ["double", "string", "vector"] {
            assert_eq!(
                entries.iter().find(|e| e.name == name).unwrap().kind,
                EntryKind::Unknown,
                "{name} 是内建定义"
            );
        }
        // 函数标记之前、且不是内建前奏的条目落在隐式类型段
        assert_eq!(
            entries.iter().find(|e| e.name == "user").unwrap().kind,
            EntryKind::Type
        );
        assert_eq!(
            entries.iter().find(|e| e.name == "getMe").unwrap().kind,
            EntryKind::Function
        );
    }

    #[test]
    fn a_capitalisation_heuristic_would_be_wrong_here() {
        // updateNewMessage 是 Update 类的构造器；首字母大写是 UpdateNewMessage ≠ Update。
        // 这个测试钉住「只能靠分区标记」这条结论。
        let entries = parse("---types---\nupdateNewMessage message:Message = Update;\n").unwrap();
        assert_eq!(entries[0].kind, EntryKind::Type);
        assert_ne!(entries[0].name, entries[0].result.to_lowercase());
    }

    #[test]
    fn entries_spanning_several_lines_are_joined() {
        let entries = parse(
            "---functions---\n\
             sendMessage chat_id:int53\n\
             \x20   input_message_content:InputMessageContent\n\
             \x20   reply_markup:ReplyMarkup = Message;\n",
        )
        .unwrap();
        assert_eq!(entries[0].params.len(), 3);
        assert_eq!(entries[0].result, "Message");
    }

    #[test]
    fn vector_and_optional_types_survive_untouched() {
        let entries = parse(
            "---functions---\ndoSomething ids:vector<int53> names:vector<string> target:Message? = Ok;\n",
        )
        .unwrap();
        assert_eq!(entries[0].params[0].ty, "vector<int53>");
        assert_eq!(entries[0].params[1].ty, "vector<string>");
        assert_eq!(entries[0].params[2].ty, "Message?");
    }

    #[test]
    fn malformed_input_is_reported_with_a_line_number() {
        let error = parse("---types---\nuser id:int53 = User;\nbroken;\n").unwrap_err();
        assert!(error.contains("第 3 行"), "{error}");
        assert!(
            parse("---types---\nuser id:int53\n").is_err(),
            "没闭合的条目要报错"
        );
    }

    #[test]
    fn a_leading_bom_does_not_break_the_first_comment() {
        let entries = parse("\u{feff}//@description hi\ngetMe = User;\n").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description.as_deref(), Some("hi"));
    }

    #[test]
    fn all_four_doc_forms_are_captured() {
        // 四种形态：主文 + //- 续行、独立字段行（含续行）、//@class、行内参数片段。
        let entries = parse(
            "---types---\n\
             //@class RichMessage @description Describes a rich message\n\
             //@description First part of the text @source Source of the message\n\
             //-Second part of the text\n\
             //@limit The limit\n\
             //-extending the limit\n\
             richMessage source:string limit:int32 = RichMessage;\n\
             ---functions---\n\
             //@description Returns a chat @chat_id Chat identifier\n\
             getChat chat_id:int53 = Chat;\n",
        )
        .unwrap();

        let rich = entries.iter().find(|e| e.name == "richMessage").unwrap();
        // 主文与 //- 续行按序拼接，行内 @source 剥进字段文档
        assert_eq!(
            rich.description.as_deref(),
            Some("First part of the text Second part of the text")
        );
        assert_eq!(
            rich.field_docs.get("source").map(String::as_str),
            Some("Source of the message")
        );
        // 独立字段行 + 它自己的 //- 续行
        assert_eq!(
            rich.field_docs.get("limit").map(String::as_str),
            Some("The limit extending the limit")
        );
        // //@class 的描述挂在条目上，供生成器汇进类文档
        assert_eq!(
            rich.class_description.as_deref(),
            Some("Describes a rich message")
        );

        let chat = entries.iter().find(|e| e.name == "getChat").unwrap();
        // 行内 @chat_id 从主文剥掉
        assert_eq!(chat.description.as_deref(), Some("Returns a chat"));
        assert_eq!(
            chat.field_docs.get("chat_id").map(String::as_str),
            Some("Chat identifier")
        );
    }

    #[test]
    fn inline_at_words_that_are_not_params_stay_in_the_description() {
        // 只有**真参数名**才会被剥：邮箱、getOption("x") 之类原样保留。
        let entries = parse(
            "---functions---\n\
             //@description Mail admin@example.com or call getOption(\"x\")\n\
             getSomething chat_id:int53 = Ok;\n",
        )
        .unwrap();
        assert_eq!(
            entries[0].description.as_deref(),
            Some("Mail admin@example.com or call getOption(\"x\")")
        );
        assert!(entries[0].field_docs.is_empty());
    }

    #[test]
    fn json_round_trips_docs_fields_too() {
        let entries = parse(
            "---types---\n\
             //@class Foo @description Foo class\n\
             //@description A foo @id The id\n\
             foo id:int53 = Foo;\n\
             ---functions---\ngetChat chat_id:int53 = Chat;\n",
        )
        .unwrap();
        let (mut back, _) = from_json(&to_json(&entries, None)).unwrap();
        // 桶顺序与入参顺序不同（函数桶在前），按名字比对
        back.sort_by(|a, b| a.name.cmp(&b.name));
        let mut expected = entries;
        expected.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(back, expected, "field_docs / class_description 必须跟着走");
    }

    #[test]
    fn json_round_trips_without_losing_anything() {
        let entries = sample();
        let json = to_json(&entries, Some("/opt/td_api.tl"));
        let (back, source) = from_json(&json).unwrap();
        assert_eq!(source.as_deref(), Some("/opt/td_api.tl"));
        // 桶顺序与入参顺序不同（函数桶在前），按名字比对
        let mut expected = entries.clone();
        expected.sort_by(|a, b| a.name.cmp(&b.name));
        let mut actual = back.clone();
        actual.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(actual, expected);
    }

    #[test]
    fn json_rejects_a_wrong_shape() {
        assert!(from_json(&json!([1, 2])).is_err());
        assert!(from_json(&json!({"functions": [{"params": []}]})).is_err());
        // 缺 result 只是空字符串，不算错
        let (entries, _) = from_json(&json!({"functions": [{"name": "x"}]})).unwrap();
        assert_eq!(entries[0].result, "");
    }
}
