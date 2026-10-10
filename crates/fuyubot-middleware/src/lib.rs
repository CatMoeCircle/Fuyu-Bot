//! 中间件管道：洋葱模型，按 stage 分组、按注册顺序执行。
//!
//! 只依赖 `fuyubot-core`。管道是**同步**的，要跑 IO 的话由中间件自己决定怎么调度。

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use fuyubot_core::{Context, Effect, KernelError};
use serde_json::Value;

/// 事件负载。
pub type Payload = Value;

/// 管道错误。
#[derive(Debug, thiserror::Error)]
pub enum MiddlewareError {
    #[error(
        "Context 里还没有 Pipeline：先 `let _guard = fuyubot_middleware::install(&ctx)?;` 并持有它"
    )]
    NotInstalled,
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// 中间件挂载点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// 收到更新、还没解析。
    PreUpdate,
    /// 路由到具体处理者之前。
    PreRoute,
    /// 插件内部处理中。
    InPlugin,
    /// 响应发出之后。
    PostResponse,
    /// 工具调用之前。
    PreToolCall,
    /// 工具调用之后。
    PostToolCall,
}

impl Stage {
    /// 全部阶段，顺序即事件流经顺序。
    pub const ALL: [Stage; 6] = [
        Stage::PreUpdate,
        Stage::PreRoute,
        Stage::InPlugin,
        Stage::PostResponse,
        Stage::PreToolCall,
        Stage::PostToolCall,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Stage::PreUpdate => "pre_update",
            Stage::PreRoute => "pre_route",
            Stage::InPlugin => "in_plugin",
            Stage::PostResponse => "post_response",
            Stage::PreToolCall => "pre_tool_call",
            Stage::PostToolCall => "post_tool_call",
        }
    }
}

/// 流过管道的那份上下文。
#[derive(Debug, Clone)]
pub struct MiddlewareContext {
    pub stage: Stage,
    pub payload: Payload,
}

impl MiddlewareContext {
    pub fn new(stage: Stage, payload: Payload) -> Self {
        Self { stage, payload }
    }
}

/// 管道里“剩下的部分”。中间件靠它把控制权交出去。
///
/// 不调用 `next.run()` 就是不往下走——这就是中断。
pub struct Next<'a> {
    layers: &'a [Arc<dyn Middleware>],
    index: usize,
}

impl Next<'_> {
    /// 执行下一层；已经是最后一层就直接返回。
    ///
    /// ponytail: 递归实现，层数 = 调用栈深度；管道上百层再改成迭代
    pub fn run(self, ctx: &mut MiddlewareContext) -> fuyubot_core::Result<()> {
        let Some(layer) = self.layers.get(self.index) else {
            return Ok(());
        };
        let next = Next {
            layers: self.layers,
            index: self.index + 1,
        };
        layer.handle(ctx, next)
    }
}

/// 一层中间件。
pub trait Middleware: Send + Sync {
    fn name(&self) -> &str;

    /// 进入段写 `next.run(ctx)` 之前，退出段写它之后。
    fn handle(&self, ctx: &mut MiddlewareContext, next: Next<'_>) -> fuyubot_core::Result<()>;
}

/// 一层挂载记录。
type Layer = (u64, Arc<dyn Middleware>);

#[derive(Default)]
struct Inner {
    next_id: u64,
    by_stage: HashMap<Stage, Vec<Layer>>,
}

/// 中间件管道。`Clone` 共享同一份挂载表。
#[derive(Clone, Default)]
pub struct Pipeline {
    inner: Arc<RwLock<Inner>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按 stage 挂载，返回的句柄 drop 即摘除。
    pub fn use_middleware(
        &self,
        stage: Stage,
        middleware: impl Middleware + 'static,
    ) -> MiddlewareHandle {
        let layer: Arc<dyn Middleware> = Arc::new(middleware);
        let name = layer.name().to_owned();
        let id;
        {
            let mut inner = self.write();
            id = inner.next_id;
            inner.next_id += 1;
            inner.by_stage.entry(stage).or_default().push((id, layer));
        }

        let pipeline = Arc::clone(&self.inner);
        let effect = Effect::new(move || {
            let mut inner = pipeline.write().unwrap_or_else(|e| e.into_inner());
            let now_empty = match inner.by_stage.get_mut(&stage) {
                Some(layers) => {
                    layers.retain(|(layer_id, _)| *layer_id != id);
                    layers.is_empty()
                }
                None => false,
            };
            if now_empty {
                inner.by_stage.remove(&stage);
            }
        });

        MiddlewareHandle {
            stage,
            name,
            effect,
        }
    }

    /// 跑一个阶段：洋葱模型，调用顺序即注册顺序。
    ///
    /// 层列表先复制出来再执行，所以中间件里再 `use_middleware`/`run` 不会自锁。
    pub fn run(&self, stage: Stage, ctx: &mut MiddlewareContext) -> fuyubot_core::Result<()> {
        let layers: Vec<Arc<dyn Middleware>> = {
            let inner = self.read();
            inner
                .by_stage
                .get(&stage)
                .map(|layers| layers.iter().map(|(_, layer)| Arc::clone(layer)).collect())
                .unwrap_or_default()
        };

        // 传进来的 stage 可能与调用者写的 payload 不符，以参数为准。
        ctx.stage = stage;
        Next {
            layers: &layers,
            index: 0,
        }
        .run(ctx)
    }

    pub fn len(&self, stage: Stage) -> usize {
        self.read().by_stage.get(&stage).map_or(0, Vec::len)
    }

    pub fn is_empty(&self, stage: Stage) -> bool {
        self.len(stage) == 0
    }

    /// 某个阶段上挂的中间件名，按注册顺序。
    pub fn names(&self, stage: Stage) -> Vec<String> {
        self.read()
            .by_stage
            .get(&stage)
            .map(|layers| layers.iter().map(|(_, l)| l.name().to_owned()).collect())
            .unwrap_or_default()
    }

    /// 中毒的锁也照用：一层中间件的 panic 不该让整条管道卡死。
    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut stages = f.debug_map();
        for stage in Stage::ALL {
            stages.entry(&stage.name(), &self.names(stage));
        }
        stages.finish()
    }
}

/// 中间件句柄。drop 即摘除。
#[must_use = "中间件句柄一被丢弃就立刻摘除刚挂上的中间件；要保留就把它存起来"]
pub struct MiddlewareHandle {
    stage: Stage,
    name: String,
    effect: Effect,
}

impl MiddlewareHandle {
    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 显式摘除。
    pub fn remove(self) {
        drop(self.effect);
    }
}

impl std::fmt::Debug for MiddlewareHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareHandle")
            .field("stage", &self.stage.name())
            .field("name", &self.name)
            .finish()
    }
}

/// 把管道装进内核 [`Context`]。
///
/// 返回的 `Effect` **必须一直持有**：它一 drop，管道就从 Context 里消失了。
pub fn install(ctx: &Context) -> Result<Effect, MiddlewareError> {
    Ok(ctx.provide(Pipeline::new())?)
}

/// `Context` 上的中间件入口。
pub trait MiddlewareContextExt {
    /// 取出装好的管道。
    fn middleware(&self) -> Result<Arc<Pipeline>, MiddlewareError>;

    /// 挂载一层中间件。
    fn use_middleware(
        &self,
        stage: Stage,
        middleware: impl Middleware + 'static,
    ) -> Result<MiddlewareHandle, MiddlewareError>;

    /// 跑某个阶段。
    fn run_middleware(&self, stage: Stage, ctx: &mut MiddlewareContext)
    -> fuyubot_core::Result<()>;
}

impl MiddlewareContextExt for Context {
    fn middleware(&self) -> Result<Arc<Pipeline>, MiddlewareError> {
        self.get::<Pipeline>()
            .map_err(|_| MiddlewareError::NotInstalled)
    }

    fn use_middleware(
        &self,
        stage: Stage,
        middleware: impl Middleware + 'static,
    ) -> Result<MiddlewareHandle, MiddlewareError> {
        Ok(self.middleware()?.use_middleware(stage, middleware))
    }

    fn run_middleware(
        &self,
        stage: Stage,
        ctx: &mut MiddlewareContext,
    ) -> fuyubot_core::Result<()> {
        self.middleware()
            .map_err(|e| -> fuyubot_core::BoxError { Box::new(e) })?
            .run(stage, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 进入段与退出段各记一笔，用来观察洋葱顺序。
    struct Tracer {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Middleware for Tracer {
        fn name(&self) -> &str {
            self.name
        }

        fn handle(&self, ctx: &mut MiddlewareContext, next: Next<'_>) -> fuyubot_core::Result<()> {
            self.log.lock().unwrap().push(format!("{}+", self.name));
            next.run(ctx)?;
            self.log.lock().unwrap().push(format!("{}-", self.name));
            Ok(())
        }
    }

    fn tracer(name: &'static str, log: &Arc<Mutex<Vec<String>>>) -> Tracer {
        Tracer {
            name,
            log: Arc::clone(log),
        }
    }

    /// 验收 4：三个中间件 A/B/C，洋葱顺序。C 是洋葱芯，进入/退出各记一次。
    #[test]
    fn three_middlewares_run_onion_style() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = Pipeline::new();
        let _a = pipeline.use_middleware(Stage::PreUpdate, tracer("A", &log));
        let _b = pipeline.use_middleware(Stage::PreUpdate, tracer("B", &log));
        let _c = pipeline.use_middleware(Stage::PreUpdate, tracer("C", &log));

        let mut ctx = MiddlewareContext::new(Stage::PreRoute, Value::Null);
        pipeline.run(Stage::PreUpdate, &mut ctx).unwrap();

        assert_eq!(*log.lock().unwrap(), ["A+", "B+", "C+", "C-", "B-", "A-"]);
        // run 以参数里的 stage 为准。
        assert_eq!(ctx.stage, Stage::PreUpdate);
    }

    #[test]
    fn not_calling_next_short_circuits_the_rest() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = Pipeline::new();

        struct Blocker {
            log: Arc<Mutex<Vec<String>>>,
        }
        impl Middleware for Blocker {
            fn name(&self) -> &str {
                "blocker"
            }
            fn handle(
                &self,
                _ctx: &mut MiddlewareContext,
                _next: Next<'_>,
            ) -> fuyubot_core::Result<()> {
                self.log.lock().unwrap().push("blocked".into());
                Ok(())
            }
        }

        let _blocker = pipeline.use_middleware(
            Stage::PreRoute,
            Blocker {
                log: Arc::clone(&log),
            },
        );
        let _after = pipeline.use_middleware(Stage::PreRoute, tracer("after", &log));

        let mut ctx = MiddlewareContext::new(Stage::PreRoute, Value::Null);
        pipeline.run(Stage::PreRoute, &mut ctx).unwrap();
        assert_eq!(*log.lock().unwrap(), ["blocked"]);
    }

    #[test]
    fn error_propagates_out_of_the_pipeline() {
        let pipeline = Pipeline::new();
        struct Failing;
        impl Middleware for Failing {
            fn name(&self) -> &str {
                "failing"
            }
            fn handle(
                &self,
                _ctx: &mut MiddlewareContext,
                _next: Next<'_>,
            ) -> fuyubot_core::Result<()> {
                Err("中间件失败".into())
            }
        }

        let log = Arc::new(Mutex::new(Vec::new()));
        let _outer = pipeline.use_middleware(Stage::PreUpdate, tracer("outer", &log));
        let _failing = pipeline.use_middleware(Stage::PreUpdate, Failing);

        let mut ctx = MiddlewareContext::new(Stage::PreUpdate, Value::Null);
        let error = pipeline.run(Stage::PreUpdate, &mut ctx).unwrap_err();
        // 外层已经被调用过；它的退出段因为 `?` 提前返回而没有执行。
        assert_eq!(*log.lock().unwrap(), ["outer+"]);
        assert!(error.to_string().contains("中间件失败"));
    }

    #[test]
    fn dropping_the_handle_removes_only_that_layer() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = Pipeline::new();
        let _a = pipeline.use_middleware(Stage::PreUpdate, tracer("A", &log));
        let b = pipeline.use_middleware(Stage::PreUpdate, tracer("B", &log));
        assert_eq!(pipeline.names(Stage::PreUpdate), ["A", "B"]);

        b.remove();
        assert_eq!(pipeline.names(Stage::PreUpdate), ["A"]);
        assert_eq!(pipeline.len(Stage::PreUpdate), 1);

        let mut ctx = MiddlewareContext::new(Stage::PreUpdate, Value::Null);
        pipeline.run(Stage::PreUpdate, &mut ctx).unwrap();
        assert_eq!(*log.lock().unwrap(), ["A+", "A-"]);
    }

    #[test]
    fn stages_are_isolated() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = Pipeline::new();
        let _pre = pipeline.use_middleware(Stage::PreToolCall, tracer("pre", &log));
        let _post = pipeline.use_middleware(Stage::PostToolCall, tracer("post", &log));

        let mut ctx = MiddlewareContext::new(Stage::PreUpdate, Value::Null);
        pipeline.run(Stage::PreToolCall, &mut ctx).unwrap();
        assert_eq!(*log.lock().unwrap(), ["pre+", "pre-"]);
        assert_eq!(pipeline.len(Stage::PostResponse), 0);
    }

    #[test]
    fn registering_from_inside_a_middleware_does_not_deadlock() {
        let pipeline = Pipeline::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        // 句柄要留在外面，否则它一离开 handle 的作用域就立刻把刚挂上的层摘了。
        let kept: Arc<Mutex<Vec<MiddlewareHandle>>> = Arc::new(Mutex::new(Vec::new()));

        struct Registering {
            pipeline: Pipeline,
            kept: Arc<Mutex<Vec<MiddlewareHandle>>>,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl Middleware for Registering {
            fn name(&self) -> &str {
                "registering"
            }
            fn handle(
                &self,
                ctx: &mut MiddlewareContext,
                next: Next<'_>,
            ) -> fuyubot_core::Result<()> {
                // 在中间件里改管道：层列表已经复制出去了，锁是空的，不该自锁。
                let added = self
                    .pipeline
                    .use_middleware(Stage::PreRoute, tracer("late", &self.log));
                self.kept.lock().unwrap().push(added);
                next.run(ctx)
            }
        }

        let _registering = pipeline.use_middleware(
            Stage::PreRoute,
            Registering {
                pipeline: pipeline.clone(),
                kept: Arc::clone(&kept),
                log: Arc::clone(&log),
            },
        );

        let mut ctx = MiddlewareContext::new(Stage::PreRoute, Value::Null);
        pipeline.run(Stage::PreRoute, &mut ctx).unwrap();

        // 本次执行看不到刚挂上的那层：层列表是执行前复制的。
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(kept.lock().unwrap().len(), 1);
        assert_eq!(pipeline.names(Stage::PreRoute), ["registering", "late"]);
    }

    #[test]
    fn context_integration() {
        let ctx = Context::new();
        let _guard = install(&ctx).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let handle = ctx
            .use_middleware(Stage::InPlugin, tracer("ctx", &log))
            .unwrap();

        let mut mw_ctx = MiddlewareContext::new(Stage::InPlugin, Value::Null);
        ctx.run_middleware(Stage::InPlugin, &mut mw_ctx).unwrap();
        assert_eq!(*log.lock().unwrap(), ["ctx+", "ctx-"]);

        drop(handle);
        assert!(ctx.middleware().unwrap().is_empty(Stage::InPlugin));

        // 没装管道时给出的错误要能指路。
        let bare = Context::new();
        let text = bare.middleware().unwrap_err().to_string();
        assert!(text.contains("fuyubot_middleware::install"), "{text}");
    }
}
