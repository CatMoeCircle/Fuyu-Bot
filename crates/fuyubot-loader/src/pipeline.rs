//! 更新分发管线：`td_receive` 的原始 JSON → 中间件 → 插件监听器。
//!
//! 一条更新的完整路径：
//!
//! ```text
//! raw_json
//!   └─ PreUpdate 中间件
//!        └─ 解析 @type
//!             └─ PreRoute 中间件
//!                  └─ InPlugin 中间件
//!                       └─ 分发给所有匹配的监听器（按 update_type + filter + 归属）
//!                            └─ PostResponse 中间件
//! ```
//!
//! 三条约定：
//! - **JSON 全程是 TDLib 原始 JSON**，一个字段都不剥；中间件可以改，改完的版本就是下游看到的版本。
//! - **中间件失败不中断管线**：记一条 warning、打一行日志，继续往下走。
//! - **插件回调 panic 不污染框架**：在 [`HostRuntime`] 那层用 `catch_unwind` 接住（回调是 `C-unwind`）。

use std::sync::Arc;

use fuyubot_event::{Filter, Payload, SubscriptionHandle};
use fuyubot_middleware::{MiddlewareContext, Stage};
use fuyubot_tdlib::{ReceiveLoop, Routed, TdClient};
use serde_json::Value;

use crate::runtime::HostRuntime;

/// 管线只在这一条更新根本没法处理时失败——中间件与监听器的报错都被吞成 warning。
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("更新不是合法 JSON: {0}")]
    NotJson(String),
    #[error("更新不是 JSON 对象")]
    NotObject,
}

/// 一条更新的处理结果。
#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    /// 从 `@type` 解析出来的更新类型（解析不出来时是 `"unknown"`）。
    pub update_type: String,
    /// 实际被调用的监听器个数。
    ///
    /// **监听器报错时是 `0`**：事件总线在第一个 `Err` 处中断广播，拿不到已经跑过几个，
    /// 而 `emit` 的返回也变成了 `Err`。那时「哪些订阅者出事了」在 `warnings` 里。
    pub listeners: usize,
    /// 报错但被吞掉的地方，按发生顺序。
    pub warnings: Vec<String>,
}

/// 更新分发管线。
///
/// 它同时是更新订阅的登记处：`subscribe` 写进去的就是管线分发时读的那张表，
/// 插件的 `HostApi.on()` 落的也是同一张表（所以插件订阅天然在管线里）。
#[derive(Clone)]
pub struct UpdatePipeline {
    runtime: Arc<HostRuntime>,
}

impl UpdatePipeline {
    pub fn new(runtime: Arc<HostRuntime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Arc<HostRuntime> {
        &self.runtime
    }

    /// 登记一条更新订阅，返回的句柄 drop 即退订。
    ///
    /// `filter` 为 `None` 表示该类型全收。
    pub fn subscribe(
        &self,
        update_type: impl Into<String>,
        filter: Option<Filter>,
        callback: impl Fn(&Payload) -> fuyubot_core::Result<()> + Send + Sync + 'static,
    ) -> SubscriptionHandle {
        self.runtime.events().on(update_type, filter, callback)
    }

    /// 退订。等价于 `drop(handle)`。
    pub fn unsubscribe(&self, handle: SubscriptionHandle) {
        self.runtime.events().off(handle);
    }

    /// 把 `TdClient` 挂进宿主运行时（**这一步不起接收循环**）。
    ///
    /// 拆成两步是为了满足 `fuyubot run` 的顺序要求：插件 `start` 期发出的请求必须能进真库
    /// （所以 client 要**先**挂上，见 AGENTS 约束 31），而更新又不能在插件加载完之前开始分发
    /// （否则插件会漏订阅）。于是「挂 client」与「开始分发」分成
    /// [`attach_tdlib`](Self::attach_tdlib) 与 [`spawn_dispatch`](Self::spawn_dispatch)。
    ///
    /// 宿主已经接过客户端时返回 `Err`（带上原来那个），不覆盖。
    pub fn attach_tdlib(self: &Arc<Self>, client: Arc<TdClient>) -> Result<(), Arc<TdClient>> {
        self.runtime.attach_tdlib(client)
    }

    /// 开始把 `td_receive` 分流进管线（第 2 步）。
    ///
    /// - `Routed::Update` → [`process_value`](Self::process_value)（六阶段 + 双通道路由）
    /// - `Routed::Response` → [`deliver_response`](Self::deliver_response)（按 `@extra` 投给发起者）
    ///
    /// **只在插件全部加载完之后调**：早调会漏更新。返回停止把手（gaps 缺口 8）——
    /// 调用方必须留好它，否则停不掉循环（`stop()` 后最多 1s 真正停下）。
    #[must_use = "不收下这个把手就永远停不掉接收循环"]
    pub fn spawn_dispatch(self: &Arc<Self>, client: &Arc<TdClient>) -> ReceiveLoop {
        let pipeline = Arc::clone(self);
        client.spawn_receive_loop(move |routed| pipeline.dispatch(routed))
    }

    /// 把接收循环分流出来的一条东西送进对应出口。[`spawn_dispatch`](Self::spawn_dispatch) 的循环调它。
    ///
    /// 这里**不 panic**：更新处理不了、响应投递失败都只打一行日志，接收循环必须继续跑。
    pub fn dispatch(&self, routed: Routed) {
        match routed {
            Routed::Update(update) => match self.process_value(update.payload) {
                Ok(outcome) => {
                    for warning in &outcome.warnings {
                        eprintln!("[fuyubot-pipeline] {warning}");
                    }
                }
                Err(e) => eprintln!("[fuyubot-pipeline] 丢掉一条处理不了的更新: {e}"),
            },
            Routed::Response(response) => {
                if let Err(e) = self.deliver_response(&response.payload) {
                    eprintln!("[fuyubot-pipeline] 投递响应失败（继续）: {e}");
                }
            }
        }
    }

    /// 处理一条原始更新。
    pub fn process(&self, raw_json: &str) -> Result<UpdateOutcome, PipelineError> {
        let payload: Value =
            serde_json::from_str(raw_json).map_err(|e| PipelineError::NotJson(e.to_string()))?;
        self.process_value(payload)
    }

    /// 处理一条已经解析好的更新。
    ///
    /// 接收循环手上本来就是 `Value`（[`Routed`]），走这条省掉「序列化回去再解析一遍」。
    pub fn process_value(&self, payload: Value) -> Result<UpdateOutcome, PipelineError> {
        if !payload.is_object() {
            return Err(PipelineError::NotObject);
        }

        let mut warnings = Vec::new();

        // a. PreUpdate
        let payload = self.stage(Stage::PreUpdate, payload, &mut warnings);

        // b. 解析更新类型
        let update_type = payload
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();

        // c. PreRoute
        let payload = self.stage(Stage::PreRoute, payload, &mut warnings);

        // d. 路由到匹配的监听器（update_type + filter + 归属都由订阅表负责）
        //    两条通道都发：具体的 @type 与兜底通道 "update"，订阅者按需选。
        // e. 每个监听器的调用在 InPlugin 之后、PostResponse 之前
        let payload = self.stage(Stage::InPlugin, payload, &mut warnings);
        let listeners = match self.runtime.publish_update(&payload) {
            Ok(count) => count,
            Err(e) => {
                // 某个监听器返回了 Err：记下来，但管线本身不算失败。
                warnings.push(format!("分发 {update_type} 时有监听器报错: {e}"));
                eprintln!("[fuyubot-pipeline] 分发 {update_type} 时有监听器报错（继续）: {e}");
                0
            }
        };
        self.stage(Stage::PostResponse, payload, &mut warnings);

        Ok(UpdateOutcome {
            update_type,
            listeners,
            warnings,
        })
    }

    /// 把一条 TDLib 响应投递给发起它的插件（详见 [`HostRuntime::deliver_response`]）。
    pub fn deliver_response(&self, payload: &Value) -> fuyubot_core::Result<usize> {
        self.runtime.deliver_response(payload)
    }

    /// 跑一个阶段，返回可能被中间件改过的 payload。失败只记 warning。
    fn stage(&self, stage: Stage, payload: Value, warnings: &mut Vec<String>) -> Value {
        let mut ctx = MiddlewareContext::new(stage, payload);
        if let Err(e) = self.runtime.middleware().run(stage, &mut ctx) {
            warnings.push(format!("{} 阶段失败: {e}", stage.name()));
            eprintln!("[fuyubot-pipeline] {} 阶段失败（继续）: {e}", stage.name());
        }
        // 失败前已经改掉的内容照样往下传。
        ctx.payload
    }
}

impl std::fmt::Debug for UpdatePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdatePipeline")
            .field("runtime", &self.runtime)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_core::Context;
    use fuyubot_middleware::{Middleware, MiddlewareHandle, Next};
    use serde_json::json;
    use std::sync::Mutex;

    /// 记录经过的阶段与 payload，也支持「顺手改一下 payload」和「故意失败」。
    struct Recorder {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        seen: Arc<Mutex<Vec<Value>>>,
        mutate: Option<(&'static str, Value)>,
        fail: bool,
    }

    impl Middleware for Recorder {
        fn name(&self) -> &str {
            self.name
        }

        fn handle(&self, ctx: &mut MiddlewareContext, next: Next<'_>) -> fuyubot_core::Result<()> {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{}", ctx.stage.name(), self.name));
            if let Some((key, value)) = &self.mutate {
                if let Some(object) = ctx.payload.as_object_mut() {
                    object.insert((*key).to_owned(), value.clone());
                }
            }
            self.seen.lock().unwrap().push(ctx.payload.clone());
            if self.fail {
                return Err("中间件故意失败".into());
            }
            next.run(ctx)
        }
    }

    fn recorder(
        runtime: &HostRuntime,
        name: &'static str,
        stage: Stage,
        log: &Arc<Mutex<Vec<String>>>,
        seen: &Arc<Mutex<Vec<Value>>>,
        mutate: Option<(&'static str, Value)>,
        fail: bool,
    ) -> MiddlewareHandle {
        runtime.middleware().use_middleware(
            stage,
            Recorder {
                name,
                log: Arc::clone(log),
                seen: Arc::clone(seen),
                mutate,
                fail,
            },
        )
    }

    struct Fixture {
        runtime: Arc<HostRuntime>,
        pipeline: UpdatePipeline,
        log: Arc<Mutex<Vec<String>>>,
        seen: Arc<Mutex<Vec<Value>>>,
        received: Arc<Mutex<Vec<Value>>>,
    }

    fn fixture() -> Fixture {
        let runtime = HostRuntime::new(Context::new()).unwrap();
        Fixture {
            pipeline: UpdatePipeline::new(Arc::clone(&runtime)),
            runtime,
            log: Arc::new(Mutex::new(Vec::new())),
            seen: Arc::new(Mutex::new(Vec::new())),
            received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 验收：一条 updateNewMessage 走完整管线，插件监听器被调用。
    #[test]
    fn a_update_walks_the_whole_pipeline() {
        let fx = fixture();
        let _pre = recorder(
            &fx.runtime,
            "pre",
            Stage::PreUpdate,
            &fx.log,
            &fx.seen,
            None,
            false,
        );
        let _route = recorder(
            &fx.runtime,
            "route",
            Stage::PreRoute,
            &fx.log,
            &fx.seen,
            None,
            false,
        );
        let _in_plugin = recorder(
            &fx.runtime,
            "in",
            Stage::InPlugin,
            &fx.log,
            &fx.seen,
            None,
            false,
        );
        let _post = recorder(
            &fx.runtime,
            "post",
            Stage::PostResponse,
            &fx.log,
            &fx.seen,
            None,
            false,
        );

        let seen_by_plugin = Arc::clone(&fx.received);
        let _listener = fx
            .pipeline
            .subscribe("updateNewMessage", None, move |payload| {
                seen_by_plugin.lock().unwrap().push(payload.clone());
                Ok(())
            });

        let outcome = fx
            .pipeline
            .process(r#"{"@type":"updateNewMessage","message":{"id":1}}"#)
            .unwrap();

        assert_eq!(outcome.update_type, "updateNewMessage");
        assert_eq!(outcome.listeners, 1);
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(
            *fx.log.lock().unwrap(),
            [
                "pre_update:pre",
                "pre_route:route",
                "in_plugin:in",
                "post_response:post"
            ]
        );
        assert_eq!(
            *fx.received.lock().unwrap(),
            [json!({"@type":"updateNewMessage","message":{"id":1}})]
        );
    }

    /// 验收：中间件改了更新之后，插件收到的是改过的版本。
    #[test]
    fn b_middleware_rewrites_reach_the_listener() {
        let fx = fixture();
        let _route = recorder(
            &fx.runtime,
            "adds-field",
            Stage::PreRoute,
            &fx.log,
            &fx.seen,
            Some(("tagged_by_middleware", json!(true))),
            false,
        );

        let seen_by_plugin = Arc::clone(&fx.received);
        let _listener = fx
            .pipeline
            .subscribe("updateNewMessage", None, move |payload| {
                seen_by_plugin.lock().unwrap().push(payload.clone());
                Ok(())
            });

        fx.pipeline
            .process(r#"{"@type":"updateNewMessage","message":{"id":2}}"#)
            .unwrap();

        let received = fx.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["tagged_by_middleware"], json!(true));
        // 原始字段一个都没少
        assert_eq!(received[0]["message"]["id"], json!(2));
    }

    #[test]
    fn c_middleware_failure_does_not_stop_the_pipeline() {
        let fx = fixture();
        let _bad = recorder(
            &fx.runtime,
            "explodes",
            Stage::PreUpdate,
            &fx.log,
            &fx.seen,
            None,
            true,
        );
        let _after = recorder(
            &fx.runtime,
            "after",
            Stage::PreRoute,
            &fx.log,
            &fx.seen,
            None,
            false,
        );

        let seen_by_plugin = Arc::clone(&fx.received);
        let _listener = fx
            .pipeline
            .subscribe("updateNewMessage", None, move |payload| {
                seen_by_plugin.lock().unwrap().push(payload.clone());
                Ok(())
            });

        let outcome = fx
            .pipeline
            .process(r#"{"@type":"updateNewMessage"}"#)
            .unwrap();

        assert_eq!(outcome.listeners, 1, "中间件失败不该挡住监听器");
        assert_eq!(outcome.warnings.len(), 1);
        assert!(
            outcome.warnings[0].contains("pre_update"),
            "{:?}",
            outcome.warnings
        );
        // 失败的那层没有调 next，所以它自己记完就断了；后面的阶段照跑。
        assert_eq!(
            *fx.log.lock().unwrap(),
            ["pre_update:explodes", "pre_route:after"]
        );
        assert_eq!(fx.received.lock().unwrap().len(), 1);
    }

    #[test]
    fn unparseable_input_is_the_only_hard_failure() {
        let fx = fixture();
        assert!(matches!(
            fx.pipeline.process("not json"),
            Err(PipelineError::NotJson(_))
        ));
        assert!(matches!(
            fx.pipeline.process("[1,2,3]"),
            Err(PipelineError::NotObject)
        ));
        // 没有 @type 也能跑，类型算 unknown
        let outcome = fx.pipeline.process(r#"{"foo":1}"#).unwrap();
        assert_eq!(outcome.update_type, "unknown");
    }

    #[test]
    fn listeners_are_filtered_by_update_type_and_filter() {
        let fx = fixture();
        let hits = Arc::new(Mutex::new(Vec::new()));
        // 句柄必须留到断言之后：它一 drop 就退订了。
        let mut handles = Vec::new();
        for (name, filter) in [
            ("by-type", None),
            (
                "by-chat",
                Some(Filter::new().field("message.chat.id", 42i64)),
            ),
        ] {
            let hits = Arc::clone(&hits);
            let name = name.to_owned();
            handles.push(fx.pipeline.subscribe("updateNewMessage", filter, move |_| {
                hits.lock().unwrap().push(name.clone());
                Ok(())
            }));
        }

        fx.pipeline
            .process(r#"{"@type":"updateNewMessage","message":{"chat":{"id":42}}}"#)
            .unwrap();
        fx.pipeline
            .process(r#"{"@type":"updateNewMessage","message":{"chat":{"id":7}}}"#)
            .unwrap();
        // 别的类型不该被调到
        fx.pipeline
            .process(r#"{"@type":"updateDeleteMessages"}"#)
            .unwrap();

        assert_eq!(
            *hits.lock().unwrap(),
            ["by-type", "by-chat", "by-type"],
            "第一次两条都中，第二次只中无过滤的那条，第三次谁都不中"
        );
        drop(handles);
    }

    #[test]
    fn response_is_the_only_thing_that_is_scoped() {
        // 更新的可见性对插件是透明的；响应才按发起者投递。这条测的是管线层面的入口。
        let fx = fixture();
        let hits = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&hits);
        let _listener = fx.pipeline.subscribe("updateNewMessage", None, move |_| {
            *seen.lock().unwrap() += 1;
            Ok(())
        });

        fx.pipeline
            .process(r#"{"@type":"updateNewMessage"}"#)
            .unwrap();
        assert_eq!(*hits.lock().unwrap(), 1);

        // 没有归属的响应按全局投递，不该被 updateNewMessage 的订阅收到
        assert_eq!(
            fx.pipeline
                .deliver_response(&json!({"@type":"ok"}))
                .unwrap(),
            0
        );
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    /// 接收循环分流出来的两种东西各走各的出口——这是 `td_receive` 接上来的那一段。
    #[test]
    fn dispatch_routes_updates_and_responses_separately() {
        use fuyubot_tdlib::{RequestId, TdResponse, TdUpdate};

        let fx = fixture();
        let updates = Arc::clone(&fx.received);
        let _updates = fx
            .pipeline
            .subscribe("updateNewMessage", None, move |payload| {
                updates.lock().unwrap().push(payload.clone());
                Ok(())
            });
        let responses = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&responses);
        let _responses = fx
            .pipeline
            .subscribe(crate::runtime::RESPONSE_EVENT, None, move |_| {
                *seen.lock().unwrap() += 1;
                Ok(())
            });

        fx.pipeline.dispatch(Routed::Update(TdUpdate {
            kind: "updateNewMessage".to_owned(),
            payload: json!({"@type":"updateNewMessage","message":{"id":9}}),
        }));
        // 更新不该被响应通道看到
        assert_eq!(*responses.lock().unwrap(), 0);

        fx.pipeline.dispatch(Routed::Response(TdResponse {
            id: RequestId::from_extra("1").unwrap(),
            payload: json!({"@type":"ok","@extra":"1"}),
        }));

        let received = fx.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["message"]["id"], json!(9));
        assert_eq!(*responses.lock().unwrap(), 1, "响应只走 response 通道");
    }
}
