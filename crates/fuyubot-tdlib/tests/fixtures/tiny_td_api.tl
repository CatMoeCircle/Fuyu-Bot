// 这是**手写的最小样例**，不是真实的 td_api.tl。
//
// 用途：Releases 里的真资产就位之前，让构建能跑起来 —— 它只覆盖生成器需要处理的那几种形状
// （互递归的类、vector、可选、关键字字段名、没有构造器的类、无参数的构造器）。
//
//   $env:TDLIB_TD_API_TL = "crates/fuyubot-tdlib/tests/fixtures/tiny_td_api.tl"
//   cargo test --workspace --offline
//
// 注意 `tests/generated_types.rs` 会直接引用这里生成出来的类型，所以改这个文件
// 等于改那三个测试的前提：里面必须有 `message` / `messageText` / `sendMessage` / `getMe`。

---types---

//@class Message @description 一条消息
message id:int53 chat_id:int53 content:MessageContent reply_to:Message is_outgoing:Bool = Message;

//@class MessageContent
messageText text:FormattedText = MessageContent;

//@class FormattedText
formattedText text:string entities:vector<TextEntity> = FormattedText;

//@class TextEntity
textEntity offset:int32 length:int32 type:string = TextEntity;

//@class User
user id:int53 type:UserType first_name:string username:string = User;

//@class UserType
userTypeRegular = UserType;

//@class Chat
chat id:int53 title:string = Chat;

//@class File
file id:int32 size:int53 local:LocalFile = File;

//@class LocalFile
localFile path:string is_downloading_completed:Bool = LocalFile;

---functions---

//@description 当前用户
getMe = User;

//@description 发消息
sendMessage chat_id:int53 message_thread_id:int53 input_message_content:InputMessageContent reply_markup:ReplyMarkup? = Message;

//@description 取聊天
getChat chat_id:int53 = Chat;

//@description 下载文件
downloadFile file_id:int32 priority:int32 offset:int53 limit:int53 = File;

//@description 回 callback query
answerCallbackQuery callback_query_id:int53 text:string show_alert:Bool = Ok;
