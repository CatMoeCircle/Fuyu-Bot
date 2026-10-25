//! 从 `td_api.tl` 生成 Rust 类型。
//!
//! 编译期由 `build.rs` 调用，产物写进 `$OUT_DIR/td_api_generated.rs`，再被
//! [`crate::td_api`] 用 `include!` 展成一个模块。**运行时不做任何生成。**
//!
//! ## 生成规则
//!
//! - 每个**类**（若干构造器共用的返回类型，例如 `Message`）→ 一个 `enum`，
//!   每个构造器是它的一个**内联结构体变体**。
//!   内联而不是「每个构造器再生成一个 struct」，是为了避开「类名」与「同名构造器」硬碰硬——
//!   TDLib 里 `user`/`User`、`message`/`Message` 就是这种关系，拆成两个类型就得给一方加后缀，
//!   命名规则立刻不统一。
//! - 每个**函数** → 一个 `struct`（参数就是字段），带 `METHOD` / `RESULT` 常量，
//!   直接喂给 `td_invoke_simplified`。
//! - **类类型的字段一律是 `Option<Box<T>>`**：`Box` 断开互递归（TL 的环一定经过类），
//!   `Option` 既对上 TDLib「字段缺失就是没有」的行为，也让 `#[serde(default)]` 成立。
//! - 认不出来的类型名降级成 `serde_json::Value`，而不是让生成的代码编译不过——
//!   生成器面对的是外部文件，遇到没见过的类型名字能退化，不能炸。
//! - Rust 关键字做转义（TDLib 里真有 `type` 这种参数名 → `r#type`）。
//!
//! ## 文档注释
//!
//! 类、变体、函数、字段**都**带文档：文本取自 `td_api.tl` 的注释（[`crate::tl_schema`]
//! 解析出的 `description` / `field_docs` / `class_description`），经 [`crate::docs`] 把
//! HTML 风格标记转成 rustdoc Markdown 后逐行发射。没有 TL 文档的字段退化成
//! `` /// `名字: 类型` `` 一行，不至于裸奔。
//!
//! ## 为什么没有分区标记就什么都不生成
//!
//! 函数与构造器只能靠 `---functions---` / `---types---` 区分（见 [`crate::tl_schema`]）。
//! 没有标记时**返回空字符串**：猜错的分类会生成一套看着像样、其实是错的 API，
//! 那比什么都没有更坏。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::docs;
use crate::tl_schema::{EntryKind, TlEntry};

/// 生成整个模块体（只有 items，交给 `include!`）。
///
/// 没有分区标记、或只有一边有内容时返回空字符串（见模块文档）。
pub fn generate(entries: &[TlEntry]) -> String {
    let functions: Vec<&TlEntry> = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Function)
        .collect();
    let constructors: Vec<&TlEntry> = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Type)
        .collect();
    if functions.is_empty() || constructors.is_empty() {
        return String::new();
    }

    let mut classes: BTreeMap<String, Vec<&TlEntry>> = BTreeMap::new();
    for constructor in &constructors {
        classes
            .entry(constructor.result.clone())
            .or_default()
            .push(constructor);
    }
    // 结果类型不是类名（脏输入）的构造器不能当类，否则会生成 `pub enum int53` 这种东西。
    classes.retain(|name, _| is_class_name(name));
    let class_names: BTreeSet<String> = classes.keys().cloned().collect();

    let mut out = String::new();
    let _ = writeln!(out, "// 本文件由 build.rs 从 td_api.tl 生成，不要手改。");
    let _ = writeln!(
        out,
        "// 条目 {} 条 → 类 {} 个、函数 {} 个。",
        entries.len(),
        classes.len(),
        functions.len()
    );

    for (class, variants) in &classes {
        generate_class(&mut out, class, variants, &class_names);
    }
    // 函数 struct 与类 enum 共用一个模块命名空间，撞名就加后缀（确定性，不靠遍历顺序）。
    let mut taken = class_names.clone();
    for function in &functions {
        generate_function(&mut out, function, &class_names, &mut taken);
    }
    out
}

fn generate_class(
    out: &mut String,
    class: &str,
    variants: &[&TlEntry],
    classes: &BTreeSet<String>,
) {
    let _ = writeln!(out);
    // 类文档：`//@class … @description …`（通常记在第一个构造器上）优先，机械行垫底。
    if let Some(description) = variants
        .iter()
        .find_map(|variant| variant.class_description.as_deref())
    {
        docs::emit_doc(out, description, "");
    }
    let _ = writeln!(
        out,
        "/// TDLib 的 `{class}` 类（{} 个构造器）。",
        variants.len()
    );
    let _ = writeln!(
        out,
        "#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]"
    );
    let _ = writeln!(out, "#[serde(tag = \"@type\")]");
    let _ = writeln!(out, "pub enum {class} {{");

    for variant in variants {
        if let Some(description) = &variant.description {
            docs::emit_doc(out, description, "    ");
        }
        let _ = writeln!(out, "    #[serde(rename = \"{}\")]", variant.name);
        let variant_name = pascal(&variant.name);
        if variant.params.is_empty() {
            let _ = writeln!(out, "    {variant_name},");
            continue;
        }
        let _ = writeln!(out, "    {variant_name} {{");
        for param in &variant.params {
            emit_field_doc(out, variant, param, "        ");
            let ty = field_type(&shape_of(&param.ty, classes));
            let _ = writeln!(out, "        {}", field_attrs(&ty));
            let rust_name = field_name(&param.name, out, "        ");
            let _ = writeln!(out, "        {rust_name}: {ty},");
        }
        let _ = writeln!(out, "    }},");
    }
    let _ = writeln!(out, "}}");
}

/// 一个字段的文档行：TL 文档（经 `docs` 转 rustdoc）优先，退化成 `` `名字: 类型` ``。
fn emit_field_doc(
    out: &mut String,
    owner: &TlEntry,
    param: &crate::tl_schema::TlParam,
    indent: &str,
) {
    if let Some(doc) = owner.field_doc(&param.name) {
        docs::emit_doc(out, doc, indent);
    } else {
        let _ = writeln!(out, "{indent}/// `{}: {}`", param.name, param.ty);
    }
}

fn generate_function(
    out: &mut String,
    function: &TlEntry,
    classes: &BTreeSet<String>,
    taken: &mut BTreeSet<String>,
) {
    let mut name = pascal(&function.name);
    while taken.contains(&name) {
        name.push('_');
    }
    taken.insert(name.clone());

    let _ = writeln!(out);
    if let Some(description) = &function.description {
        docs::emit_doc(out, description, "");
    }
    let _ = writeln!(
        out,
        "/// TDLib 方法 `{}`，结果是 `{}`。",
        function.name, function.result
    );
    let _ = writeln!(
        out,
        // 函数 struct 要 Default：构造请求时 `..Default::default()` 是常态。
        // 类 enum 不能派生 Default（得指定哪个变体是默认），所以两边不一样。
        "#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]"
    );
    if function.params.is_empty() {
        let _ = writeln!(out, "pub struct {name} {{}}");
    } else {
        let _ = writeln!(out, "pub struct {name} {{");
        for param in &function.params {
            emit_field_doc(out, function, param, "    ");
            let ty = field_type(&shape_of(&param.ty, classes));
            let _ = writeln!(out, "    {}", field_attrs(&ty));
            let rust_name = field_name(&param.name, out, "    ");
            let _ = writeln!(out, "    pub {rust_name}: {ty},");
        }
        let _ = writeln!(out, "}}");
    }
    let _ = writeln!(out, "impl {name} {{");
    let _ = writeln!(
        out,
        "    /// 直接喂给 `td_invoke` / `td_invoke_simplified` 的方法名。"
    );
    let _ = writeln!(
        out,
        "    pub const METHOD: &'static str = \"{}\";",
        function.name
    );
    let _ = writeln!(out, "    /// TDLib 文档里写的返回类型。");
    let _ = writeln!(
        out,
        "    pub const RESULT: &'static str = \"{}\";",
        function.result
    );
    let _ = writeln!(out, "}}");
}

/// 字段上的 serde 属性。缺字段一律取默认值（TDLib 会省略缺省项）。
///
/// 反过来，默认值也不该被写进请求：`Option` / `Vec` / 降级成 `Value` 的字段
/// 在没填时跳过，免得给 TDLib 发一堆 `null` 和 `[]`。
fn field_attrs(ty: &str) -> String {
    let skip = if ty.starts_with("Option<") {
        ", skip_serializing_if = \"Option::is_none\""
    } else if ty.starts_with("Vec<") {
        ", skip_serializing_if = \"Vec::is_empty\""
    } else if ty == "serde_json::Value" || ty == "Option<serde_json::Value>" {
        ", skip_serializing_if = \"serde_json::Value::is_null\""
    } else {
        ""
    };
    format!("#[serde(default{skip})]")
}

/// TL 类型 → 生成用的形状。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    Primitive(&'static str),
    Class(String),
    /// 认不出来的类型名（含没有构造器的类）。
    Unknown,
    Vector(Box<Shape>),
    Optional(Box<Shape>),
}

fn shape_of(ty: &str, classes: &BTreeSet<String>) -> Shape {
    let ty = ty.trim();
    if let Some(inner) = ty
        .strip_prefix("vector<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return Shape::Vector(Box::new(shape_of(inner, classes)));
    }
    if let Some(inner) = ty.strip_suffix('?') {
        return Shape::Optional(Box::new(shape_of(inner, classes)));
    }
    match ty {
        "int32" => Shape::Primitive("i32"),
        "int53" | "int64" => Shape::Primitive("i64"),
        "double" => Shape::Primitive("f64"),
        // TDLib 的 JSON 里 bytes 就是 base64 字符串
        "string" | "bytes" => Shape::Primitive("String"),
        "bool" | "Bool" => Shape::Primitive("bool"),
        other if classes.contains(other) => Shape::Class(other.to_owned()),
        _ => Shape::Unknown,
    }
}

/// 字段类型。顶层直接出现的类要 `Option<Box<T>>`（见模块文档）。
fn field_type(shape: &Shape) -> String {
    match shape {
        Shape::Primitive(primitive) => (*primitive).to_owned(),
        Shape::Unknown => "serde_json::Value".to_owned(),
        Shape::Class(class) => format!("Option<Box<{class}>>"),
        Shape::Optional(inner) => match &**inner {
            Shape::Class(class) => format!("Option<Box<{class}>>"),
            other => format!("Option<{}>", plain_type(other)),
        },
        Shape::Vector(inner) => format!("Vec<{}>", plain_type(inner)),
    }
}

/// 不装箱的渲染：`Vec`/`Option` 已经是堆上的，里面的类不必再 `Box`。
fn plain_type(shape: &Shape) -> String {
    match shape {
        Shape::Primitive(primitive) => (*primitive).to_owned(),
        Shape::Unknown => "serde_json::Value".to_owned(),
        Shape::Class(class) => class.clone(),
        Shape::Optional(inner) => format!("Option<{}>", plain_type(inner)),
        Shape::Vector(inner) => format!("Vec<{}>", plain_type(inner)),
    }
}

/// 类名是 PascalCase；`int53`、`string` 这种结果类型不是类。
fn is_class_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// `updateNewMessage` → `UpdateNewMessage`，`user` → `User`。
fn pascal(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper_next = true;
    for ch in name.chars() {
        if ch == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// 字段名转义。这四个不能写成裸标识符（`r#self` 之类不合法），只能加下划线。
const UNRAWABLE: &[&str] = &["self", "Self", "super", "crate"];
const KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl", "in", "let", "loop",
    "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return", "static",
    "struct", "trait", "true", "try", "type", "typeof", "union", "unsafe", "unsized", "use",
    "virtual", "where", "while", "yield",
];

/// 把 TL 的参数名写成合法的 Rust 字段名，必要时补 `#[serde(rename)]`。
fn field_name(name: &str, out: &mut String, indent: &str) -> String {
    let rust = if UNRAWABLE.contains(&name) {
        format!("{name}_")
    } else if KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    };
    // 裸标识符 serde 认的是去掉 `r#` 之后的名字，只有加下划线那种才需要改名。
    if rust != name && rust != format!("r#{name}") {
        let _ = writeln!(out, "{indent}#[serde(rename = \"{name}\")]");
    }
    rust
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tl_schema;

    /// 故意挑刺的样例：互递归、关键字字段名、vector、可选、空类、未知类型、空参数函数。
    const SAMPLE: &str = "---types---\n\
         //@class Message @description Describes a message\n\
         message id:int53 content:MessageContent reply_to:Message is_premium:Bool = Message;\n\
         messageText text:FormattedText = MessageContent;\n\
         formattedText text:string entities:vector<TextEntity> = FormattedText;\n\
         textEntity offset:int32 length:int32 = TextEntity;\n\
         //@class User\n\
         user id:int53 type:UserType is_premium:Bool = User;\n\
         userTypeRegular = UserType;\n\
         ---functions---\n\
         getMe = User;\n\
         sendMessage chat_id:int53 input_message_content:InputMessageContent reply_markup:ReplyMarkup? = Message;\n\
         downloadFile file_id:int32 ids:vector<int53> = File;\n";

    fn generated() -> String {
        let entries = tl_schema::parse(SAMPLE).unwrap();
        generate(&entries)
    }

    #[test]
    fn every_class_becomes_an_enum_tagged_by_at_type() {
        let code = generated();
        assert!(code.contains("#[serde(tag = \"@type\")]"), "{code}");
        for class in [
            "Message",
            "MessageContent",
            "FormattedText",
            "User",
            "UserType",
        ] {
            assert!(
                code.contains(&format!("pub enum {class} {{")),
                "缺少 {class}：{code}"
            );
        }
        // 构造器是内联结构体变体，带 @type 重命名
        assert!(code.contains("#[serde(rename = \"message\")]"), "{code}");
        assert!(code.contains("    Message {"), "{code}");
        assert!(
            code.contains("#[serde(rename = \"userTypeRegular\")]"),
            "{code}"
        );
        // 无参数的构造器是单元变体
        assert!(code.contains("    UserTypeRegular,"), "{code}");
    }

    #[test]
    fn recursive_class_fields_are_boxed() {
        let code = generated();
        // message.reply_to:Message —— 不装箱就是无限大小（变体字段不能有 pub）
        assert!(
            code.contains("        reply_to: Option<Box<Message>>,"),
            "{code}"
        );
        assert!(
            code.contains("        content: Option<Box<MessageContent>>,"),
            "{code}"
        );
        // Vec 已经是堆上的，里面不再装箱
        assert!(
            code.contains("        entities: Vec<TextEntity>,"),
            "{code}"
        );
    }

    #[test]
    fn rust_keywords_and_unknown_types_degrade_safely() {
        let code = generated();
        // TDLib 真的有 `type` 这种参数名
        assert!(
            code.contains("        r#type: Option<Box<UserType>>,"),
            "{code}"
        );
        // 没有构造器的类（样例里的 InputMessageContent）降级成 Value，而不是编译不过
        assert!(
            code.contains("    pub input_message_content: serde_json::Value,"),
            "认不出的类型应降级成 Value：{code}"
        );
        assert!(
            code.contains("    pub reply_markup: Option<serde_json::Value>,"),
            "{code}"
        );
        // 降级字段默认是 null，不该被写进请求
        assert!(
            code.contains("skip_serializing_if = \"serde_json::Value::is_null\""),
            "{code}"
        );
    }

    #[test]
    fn functions_become_structs_with_their_method_name() {
        let code = generated();
        assert!(code.contains("pub struct GetMe {}"), "{code}");
        assert!(code.contains("pub struct SendMessage {"), "{code}");
        assert!(
            code.contains("pub const METHOD: &'static str = \"sendMessage\";"),
            "{code}"
        );
        assert!(
            code.contains("pub const RESULT: &'static str = \"Message\";"),
            "{code}"
        );
        // 可选字段（这里 reply_markup 的类没有构造器，所以是 Value）
        assert!(code.contains("skip_serializing_if"), "{code}");
        // 缺字段要取默认值
        assert!(code.contains("#[serde(default"), "{code}");
    }

    #[test]
    fn docs_become_rustdoc_markdown_on_classes_variants_and_fields() {
        let entries = tl_schema::parse(
            "---types---\n\
             //@class Foo @description Describes a foo\n\
             //@description A foo with <b>style</b> @id The identifier\n\
             //@extra Something extra\n\
             foo id:int53 extra:string = Foo;\n\
             ---functions---\n\
             //@description Uses [brackets] and <code>code</code>.\n\
             //@chat_id Chat identifier\n\
             getChat chat_id:int53 = Foo;\n",
        )
        .unwrap();
        let code = generate(&entries);

        // 类描述（//@class）进 enum 文档
        assert!(code.contains("/// Describes a foo"), "{code}");
        // HTML 标记转成 rustdoc Markdown；方括号**不**转义（链接要活着）
        assert!(code.contains("/// A foo with **style**"), "{code}");
        assert!(code.contains("/// Uses [brackets] and `code`."), "{code}");
        // 独立字段行进字段文档
        assert!(code.contains("/// Something extra"), "{code}");
        assert!(code.contains("/// Chat identifier"), "{code}");
        // 行内 `@参数 …` 片段剥进字段文档，主文里不再留尾巴
        assert!(code.contains("/// The identifier"), "{code}");
        assert!(!code.contains("@id The"), "{code}");
    }

    #[test]
    fn fields_without_tl_docs_fall_back_to_the_name_type_line() {
        let entries = tl_schema::parse(
            "---types---\nuser id:int53 = User;\n---functions---\n\
             //@description Sends a message\n\
             sendMessage chat_id:int53 = Message;\n",
        )
        .unwrap();
        let code = generate(&entries);
        // 没有 TL 文档的字段不能裸奔
        assert!(code.contains("        /// `id: int53`"), "{code}");
        assert!(code.contains("    /// `chat_id: int53`"), "{code}");
    }

    #[test]
    fn nothing_is_generated_without_section_markers() {
        // 判不了函数/构造器时宁可什么都不生成，也不靠猜
        assert_eq!(
            generate(&tl_schema::parse("getMe = User;\nuser id:int53 = User;\n").unwrap()),
            ""
        );
        // 只有一边也不行
        assert_eq!(
            generate(&tl_schema::parse("---functions---\ngetMe = User;\n").unwrap()),
            ""
        );
        assert_eq!(
            generate(&tl_schema::parse("---types---\nuser id:int53 = User;\n").unwrap()),
            ""
        );
    }

    #[test]
    fn generated_names_never_collide_across_classes_and_functions() {
        // 函数 struct 与类 enum 共用命名空间：撞名要有确定性的处理
        let entries =
            tl_schema::parse("---types---\nuser id:int53 = User;\n---functions---\nuser = User;\n")
                .unwrap();
        let code = generate(&entries);
        assert!(code.contains("pub enum User {"), "{code}");
        assert!(code.contains("pub struct User_ {"), "撞名要加后缀：{code}");
    }
}
