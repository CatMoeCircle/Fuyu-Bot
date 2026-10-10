//! 编译期生成的 TDLib 类型的自检。
//!
//! 这些类型来自 build.rs 拿到的 `td_api.tl`（真实的或 `TDLIB_TD_API_TL` 指的那份）——
//! 拿不到时构建就已经失败了，所以这里可以无条件引用它们。
//!
//! ```powershell
//! $env:TDLIB_TD_API_TL = "C:\path\to\td_api.tl"
//! cargo test -p fuyubot-tdlib --test generated_types
//! ```
//!
//! 断言故意只依赖 `td_api.tl` 里**一定存在**的东西（`message` / `messageText` / `id`），
//! 所以真文件与样例文件都能跑。

use fuyubot_tdlib::td_api;

/// TDLib 发过来的 JSON 能直接变成生成出来的 Rust 类型。
#[test]
fn tdlib_json_deserializes_into_generated_types() {
    let raw = serde_json::json!({
        "@type": "message",
        "id": 7,
        "content": {
            "@type": "messageText",
            "text": { "@type": "formattedText", "text": "hi", "entities": [] }
        },
        // TDLib 加新字段是常态，不能因此炸
        "some_field_from_a_newer_tdlib": 1
    });

    let message: td_api::Message = serde_json::from_value(raw).expect("应当能反序列化");
    let td_api::Message::Message { id, content, .. } = &message;

    assert_eq!(*id, 7);
    let content = content.as_deref().expect("content 应当解析出来");
    assert!(matches!(
        content,
        td_api::MessageContent::MessageText { .. }
    ));
}

/// 往返之后仍是 TDLib 认得的形状（`@type` 在，多余字段不会回写）。
#[test]
fn generated_types_serialize_back_with_at_type() {
    let raw = serde_json::json!({
        "@type": "message",
        "id": 7,
        "content": { "@type": "messageText" },
        "some_field_from_a_newer_tdlib": 1
    });
    let message: td_api::Message = serde_json::from_value(raw).unwrap();
    let back = serde_json::to_value(&message).unwrap();

    assert_eq!(back["@type"], "message", "{back}");
    assert_eq!(back["id"], 7, "{back}");
    assert!(
        back.get("some_field_from_a_newer_tdlib").is_none(),
        "没进模型的多余字段不该被回写：{back}"
    );
}

/// 函数 struct 带得动方法名，能直接喂给 `td_invoke_simplified`。
#[test]
fn function_structs_carry_their_method_name() {
    assert_eq!(td_api::SendMessage::METHOD, "sendMessage");
    assert_eq!(td_api::GetMe::METHOD, "getMe");
    assert!(!td_api::SendMessage::RESULT.is_empty());

    let request = td_api::SendMessage {
        chat_id: 42,
        ..Default::default()
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["chat_id"], 42);
    // 没填的字段不该出现在请求里（TDLib 按缺省处理）
    assert!(json.get("input_message_content").is_none(), "{json}");
}
