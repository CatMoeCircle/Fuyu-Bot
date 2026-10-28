//! [`PluginManager`]：Fiber 生命周期、依赖注入与卸载回滚。
//!
//! 这一版把「**加载顺序**」换成了「**可用性**」（Cordis 的做法）：
//! 插件不必「先加载依赖再加载我」，而是声明它要哪些能力（[`Plugin::inject`]），
//! **依赖齐了才跑 `apply`**，在那之前它停在 Pending——那是合法状态，不是错误。
//!
//! 依赖是**持续跟踪**的：某个能力被注销了，依赖它的插件自动退回 Pending
//! （它已生效的注册一并回滚）；能力回来时再自动重新激活。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::context::Context;
use crate::effect::Effect;
use crate::error::KernelError;
use crate::plugin::Plugin;

/// 激活序号的来源。只用来**排序**（谁先活、谁后活），跨 `Context` 共享也无所谓。
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

/// 一个 fiber（一次插件安装）的状态。
enum State {
    /// 依赖还没齐，等着。**这不是失败。**
    Pending,
    /// `apply` 正在跑。标出来是为了让重进来的 reconcile 不再对它下手
    /// （`apply` 里往往会注册服务，那会同步触发一次 reconcile）。
    Activating,
    /// 活着。`effect` 是 `apply` 交回来的回滚守卫。
    ///
    /// 这个字段**不会被读**：它存在的全部意义是 drop 时触发回滚（见 `dispose`）。
    #[allow(dead_code)]
    Active { effect: Effect },
    /// `apply` 返回了错误。别的插件不受影响。
    Failed { reason: String },
}

impl State {
    fn is_active(&self) -> bool {
        matches!(self, State::Active { .. })
    }
}

/// 一次插件安装。
struct Fiber {
    plugin: Arc<dyn Plugin>,
    state: State,
    /// 最近一次被激活的序号（没被激活过就是 `None`）。`active()` 靠它按
    /// **激活先后**汇报，而不是按登记先后——后者会让人误以为那是加载顺序。
    seq: Option<u64>,
}

impl Fiber {
    fn name(&self) -> &str {
        self.plugin.name()
    }

    /// 它要的能力是不是都到位了。
    ///
    /// 一个名字有两种满足方式，这正是「按插件依赖」与「按能力依赖」能共存的理由：
    /// - 服务表里有这个名字（某个插件提供的能力，跨语言也查得到）；
    /// - 有个同名 fiber 正活着（依赖的那个**具体插件**已经起来了）。
    fn requirements_met(&self, ctx: &Context, fibers: &[Fiber]) -> bool {
        self.plugin
            .inject()
            .iter()
            .all(|capability| is_available(ctx, fibers, capability))
    }

    /// 还缺哪几个能力（诊断用）。
    fn missing(&self, ctx: &Context, fibers: &[Fiber]) -> Vec<String> {
        self.plugin
            .inject()
            .into_iter()
            .filter(|capability| !is_available(ctx, fibers, capability))
            .collect()
    }
}

/// 一个能力的名字有没有被满足。
fn is_available(ctx: &Context, fibers: &[Fiber], capability: &str) -> bool {
    ctx.has(capability)
        || fibers
            .iter()
            .any(|other| other.state.is_active() && other.name() == capability)
}

/// 插件管理器。
///
/// 持有 [`Context`]，并在它上面挂了服务变更钩子：能力出现/消失时自动重新求值
/// 所有 fiber。所有注册都落在那个 [`Context`] 上。
pub struct PluginManager {
    ctx: Context,
    /// 按登记顺序排列。`Mutex` 是为了让服务变更钩子也能碰它。
    fibers: Arc<Mutex<Vec<Fiber>>>,
}

impl PluginManager {
    pub fn new(ctx: Context) -> Self {
        let fibers = Arc::new(Mutex::new(Vec::new()));

        // 能力一增删就重新求值：这是「依赖持续跟踪」的驱动源。
        // 持**弱**引用——钩子被 Context 拿着，这里再持强引用双方都放不了手。
        let weak = Arc::downgrade(&fibers);
        let ctx_for_hook = ctx.clone();
        ctx.on_service_change(move |_name, _change| {
            let Some(fibers) = weak.upgrade() else {
                return;
            };
            reconcile_if_free(&ctx_for_hook, &fibers);
        });

        Self { ctx, fibers }
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// 登记了就算已加载（哪怕还在等依赖）。
    pub fn is_loaded(&self, name: &str) -> bool {
        self.lock_fibers().iter().any(|fiber| fiber.name() == name)
    }

    /// 已登记插件的名字，按登记顺序。
    pub fn loaded(&self) -> Vec<String> {
        self.lock_fibers()
            .iter()
            .map(|fiber| fiber.name().to_owned())
            .collect()
    }

    /// **真正跑起来了**的那些（依赖齐 + `apply` 成功）。
    pub fn active(&self) -> Vec<String> {
        let mut live: Vec<(u64, String)> = self
            .lock_fibers()
            .iter()
            .filter(|fiber| fiber.state.is_active())
            .map(|fiber| (fiber.seq.unwrap_or(0), fiber.name().to_owned()))
            .collect();
        live.sort_by_key(|(seq, _)| *seq);
        live.into_iter().map(|(_, name)| name).collect()
    }

    /// 还卡在 Pending 的：`(插件名, 缺的能力)`。
    ///
    /// 「我的插件怎么什么都没干」的第一现场就在这里。
    pub fn pending(&self) -> Vec<(String, Vec<String>)> {
        let fibers = self.lock_fibers();
        fibers
            .iter()
            .filter(|fiber| matches!(fiber.state, State::Pending))
            .map(|fiber| {
                let missing = fiber.missing(&self.ctx, &fibers);
                (fiber.name().to_owned(), missing)
            })
            .collect()
    }

    /// `apply` 失败的名字与原因。
    pub fn failed(&self) -> Vec<(String, String)> {
        self.lock_fibers()
            .iter()
            .filter_map(|fiber| match &fiber.state {
                State::Failed { reason } => Some((fiber.name().to_owned(), reason.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.lock_fibers().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock_fibers().is_empty()
    }

    /// 登记一个插件。依赖齐就立刻激活，不齐就停在 Pending——**不会报错**。
    pub fn load(&mut self, plugin: impl Plugin + 'static) -> Result<String, KernelError> {
        self.load_shared(Arc::new(plugin))
    }

    /// 同上，但对已经是 `Arc` 的插件。
    pub fn load_shared(&mut self, plugin: Arc<dyn Plugin>) -> Result<String, KernelError> {
        let name = plugin.name().to_owned();
        {
            let mut fibers = self.lock_fibers();
            if fibers.iter().any(|fiber| fiber.name() == name) {
                return Err(KernelError::PluginAlreadyLoaded(name));
            }
            fibers.push(Fiber {
                plugin,
                state: State::Pending,
                seq: None,
            });
        }
        self.reconcile();
        Ok(name)
    }

    /// 登记一批插件，返回**这一次真正激活的**，按激活顺序。
    ///
    /// 剩下的留在 Pending（问我 [`pending`](Self::pending) / [`failed`](Self::failed)）。
    /// 调用方不必再操心传入顺序——谁先谁后都一样，依赖说了算。
    pub fn load_all(&mut self, plugins: Vec<Arc<dyn Plugin>>) -> Result<Vec<String>, KernelError> {
        {
            let mut fibers = self.lock_fibers();
            let mut seen: HashMap<String, ()> = HashMap::with_capacity(plugins.len());
            for plugin in plugins {
                let name = plugin.name().to_owned();
                if fibers.iter().any(|fiber| fiber.name() == name)
                    || seen.insert(name.clone(), ()).is_some()
                {
                    return Err(KernelError::DuplicatePlugin(name));
                }
                fibers.push(Fiber {
                    plugin,
                    state: State::Pending,
                    seq: None,
                });
            }
        }
        Ok(self.reconcile())
    }

    /// 卸载：依赖它的先退回去，然后才 dispose 它自己。
    ///
    /// 顺序是刻意的：Cordis 里「依赖方先撤」才能保证没人攥着已失效的东西，
    /// 而且和 `Effect` 逆序回滚的直觉一致（后起来的先走）。
    pub fn unload(&mut self, name: &str) -> Result<(), KernelError> {
        // 先把它从表里摘掉——这样下面的 reconcile 就看不到它了，依赖方会自行退回。
        let taken = {
            let mut fibers = self.lock_fibers();
            let Some(position) = fibers.iter().position(|fiber| fiber.name() == name) else {
                return Err(KernelError::PluginNotLoaded(name.to_owned()));
            };
            fibers.remove(position)
        };
        self.reconcile();
        dispose(&self.ctx, taken);
        Ok(())
    }

    /// 全部卸载，返回卸载顺序：**激活顺序的倒序**（后起来的先走）。
    ///
    /// 按激活序号倒着来，而不是按登记顺序倒着 pop——登记顺序是巧合，
    /// 激活顺序才反映「谁依赖谁」。
    pub fn unload_all(&mut self) -> Result<Vec<String>, KernelError> {
        let mut names = Vec::new();
        loop {
            // 显式花括号把锁收在这一段里：出括号就放开，别一路攥到 dispose。
            let taken = {
                let mut fibers = self.lock_fibers();
                let Some(index) = fibers
                    .iter()
                    .enumerate()
                    // 还没激活过的（Pending）seq 是 None，算最小值，最先走。
                    .max_by_key(|(_, fiber)| fiber.seq.unwrap_or(0))
                    .map(|(index, _)| index)
                else {
                    break;
                };
                fibers.remove(index)
            };
            names.push(taken.name().to_owned());
            dispose(&self.ctx, taken);
        }
        Ok(names)
    }

    /// 卸载再加载。依赖齐的话它会当场重新激活。
    pub fn reload(&mut self, name: &str) -> Result<String, KernelError> {
        let Some(plugin) = self
            .lock_fibers()
            .iter()
            .find(|fiber| fiber.name() == name)
            .map(|fiber| Arc::clone(&fiber.plugin))
        else {
            return Err(KernelError::PluginNotLoaded(name.to_owned()));
        };

        self.unload(name)?;
        self.load_shared(plugin)
    }

    /// 手动驱动一次依赖重算。正常由服务变更钩子自动触发。
    ///
    /// 返回这次**新激活**的顺序。
    pub fn reconcile(&mut self) -> Vec<String> {
        reconcile_loop(&self.ctx, &self.fibers)
    }

    fn lock_fibers(&self) -> MutexGuard<'_, Vec<Fiber>> {
        self.fibers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 钩子侧的入口：抢不到锁说明正有人在 reconcile（`apply` 里注册了服务就会这样），
/// 那一层的 loop 会把剩下的推进完，这里不必逞强。
fn reconcile_if_free(ctx: &Context, fibers: &Mutex<Vec<Fiber>>) {
    let Ok(guard) = fibers.try_lock() else {
        return;
    };
    drop(guard);
    reconcile_loop(ctx, fibers);
}

/// 反复推进直到没有状态要变。返回**这次**新激活的顺序。
fn reconcile_loop(ctx: &Context, fibers: &Mutex<Vec<Fiber>>) -> Vec<String> {
    let mut activated = Vec::new();
    // 保险丝：一轮最多推进一个 fiber。给足冗余再撒手，免得依赖图万一构了环
    // 把这里卡成死循环。
    let budget = fibers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
        .saturating_mul(2)
        .saturating_add(16);

    for _ in 0..budget {
        let Ok(mut guard) = fibers.lock() else { break };

        // 先处理「依赖不再满足、得退回去」的，再处理「可以起来了」的。
        let mut next = None;
        for (index, fiber) in guard.iter().enumerate() {
            if fiber.state.is_active() && !fiber.requirements_met(ctx, &guard) {
                next = Some((index, Step::Deactivate));
                break;
            }
        }
        if next.is_none() {
            for (index, fiber) in guard.iter().enumerate() {
                if matches!(fiber.state, State::Pending) && fiber.requirements_met(ctx, &guard) {
                    next = Some((index, Step::Activate));
                    break;
                }
            }
        }
        let Some((index, step)) = next else { break };

        match step {
            Step::Activate => {
                // Arc 先拷出来：`apply` 期间不能还攥着 fibers 的借用。
                let plugin = Arc::clone(&guard[index].plugin);
                let name = plugin.name().to_owned();
                guard[index].state = State::Activating;
                drop(guard);

                // apply 多半会注册服务 → 同步触发钩子 → 钩子 try_lock 失败就返回，
                // 不会和这里的推进打架。
                let outcome = plugin.apply(ctx);

                let mut guard = fibers.lock().unwrap_or_else(|e| e.into_inner());
                let next_state = match outcome {
                    Ok(effect) => {
                        activated.push(name);
                        State::Active { effect }
                    }
                    Err(error) => State::Failed {
                        reason: error.to_string(),
                    },
                };
                if next_state.is_active() {
                    guard[index].seq = Some(NEXT_SEQ.fetch_add(1, Ordering::Relaxed));
                }
                guard[index].state = next_state;
            }
            Step::Deactivate => {
                let previous = std::mem::replace(&mut guard[index].state, State::Pending);
                guard[index].seq = None;
                let plugin = Arc::clone(&guard[index].plugin);
                drop(guard);

                plugin.dispose(ctx);
                drop(previous);
            }
        }
    }

    activated
}

/// 一次推进的种类。
enum Step {
    Activate,
    Deactivate,
}

/// 把一个 fiber 彻底收起来：`dispose` 之后再 drop 守卫触发回滚。
fn dispose(ctx: &Context, fiber: Fiber) {
    let Fiber { plugin, state, .. } = fiber;
    plugin.dispose(ctx);
    drop(state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Debug, PartialEq)]
    struct Greeting(&'static str);

    /// 无状态插件 + 注册一个服务，用来验证「卸载 → 服务消失」。
    struct Greeter;
    impl Plugin for Greeter {
        fn name(&self) -> &str {
            "greeter"
        }
        fn apply(&self, ctx: &Context) -> Result<Effect, KernelError> {
            ctx.provide(Greeting("hi"))
        }
    }

    /// 有名字、依赖与 dispose 记录的插件。
    struct Named {
        name: &'static str,
        deps: Vec<String>,
        log: Arc<StdMutex<Vec<String>>>,
    }
    impl Named {
        fn new(
            name: &'static str,
            deps: &[&str],
            log: &Arc<StdMutex<Vec<String>>>,
        ) -> Arc<dyn Plugin> {
            Arc::new(Self {
                name,
                deps: deps.iter().map(|d| (*d).to_owned()).collect(),
                log: Arc::clone(log),
            })
        }
    }
    impl Plugin for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn dependencies(&self) -> Vec<String> {
            self.deps.clone()
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Ok(Effect::none())
        }
        fn dispose(&self, _ctx: &Context) {
            self.log.lock().unwrap().push(self.name.to_owned());
        }
    }

    /// `apply` 直接报错的插件。
    struct Failing;
    impl Plugin for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Err(KernelError::ServiceMissing("apply 故意失败"))
        }
    }

    #[test]
    fn load_provide_unload_makes_the_service_disappear() {
        let ctx = Context::new();
        let mut manager = PluginManager::new(ctx.clone());

        assert_eq!(manager.load(Greeter).unwrap(), "greeter");
        assert!(manager.is_loaded("greeter"));
        assert_eq!(*ctx.get::<Greeting>().unwrap(), Greeting("hi"));

        manager.unload("greeter").unwrap();
        assert!(!manager.is_loaded("greeter"));
        assert!(matches!(
            ctx.get::<Greeting>(),
            Err(KernelError::ServiceMissing(_))
        ));
    }

    /// 激活顺序由依赖决定，与传入顺序无关——但**机理变了**：不是排的序，
    /// 而是 B 在等 A 出现，A 一起来 B 就跟上。
    #[test]
    fn dependencies_decide_activation_order_not_argument_order() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        // 故意把依赖者排在前面。
        let batch = vec![Named::new("B", &["A"], &log), Named::new("A", &[], &log)];
        assert_eq!(manager.load_all(batch).unwrap(), ["A", "B"]);
        assert_eq!(manager.active(), ["A", "B"]);

        assert_eq!(manager.unload_all().unwrap(), ["B", "A"]);
        assert_eq!(*log.lock().unwrap(), ["B", "A"]);
    }

    /// 缺依赖不再是错误——只是 Pending。这是 Cordis 与「拓扑排序」最根本的分歧。
    #[test]
    fn a_missing_dependency_means_pending_not_failure() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        manager.load_shared(Named::new("B", &["A"], &log)).unwrap();
        assert_eq!(manager.loaded(), ["B"], "登记了就算已加载");
        assert!(manager.active().is_empty(), "依赖没齐就不该跑");

        let waiting = manager.pending();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].0, "B");
        assert_eq!(waiting[0].1, ["A"], "应该报得出在等谁");

        // A 一到，B 自己就起来了，不需要谁再去「重新加载」它。
        // active() 按激活先后汇报：先 A（B 在等它），再 B。
        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        assert_eq!(manager.active(), ["A", "B"]);
        assert!(manager.pending().is_empty());
    }

    /// 依赖消失时，依赖方自动退回 Pending 并被 dispose：
    /// 这才是「运行中的消费方不会攥着一个已经不可用的服务」的保证。
    #[test]
    fn losing_a_dependency_unloads_the_dependent() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        manager.load_shared(Named::new("B", &["A"], &log)).unwrap();
        assert_eq!(manager.active(), ["A", "B"]);

        manager.unload("A").unwrap();
        assert!(manager.active().is_empty(), "A 走了，B 不该还在跑");

        let waiting = manager.pending();
        assert_eq!(waiting[0].0, "B");
        assert_eq!(
            *log.lock().unwrap(),
            ["B", "A"],
            "B 先被收起来，然后才是被显式卸载的 A"
        );
    }

    /// 依赖去而复返：A 被卸载后 B 退回 Pending，A 回来时 B 自动重新激活。
    #[test]
    fn dependents_come_back_when_the_dependency_returns() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        manager.load_shared(Named::new("B", &["A"], &log)).unwrap();
        assert_eq!(manager.active(), ["A", "B"]);

        manager.unload("A").unwrap();
        assert_eq!(manager.pending().len(), 1);

        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        assert_eq!(manager.active(), ["A", "B"], "B 不该需要手动重加载");
    }

    /// `inject` 写**能力名**（而不是插件名）同样成立，且与到达顺序无关——
    /// 这正是 Cordis 里「消费者按能力索取、提供者可替换」的落点。
    #[test]
    fn injecting_a_capability_name_is_order_independent() {
        struct Consumer;
        impl Plugin for Consumer {
            fn name(&self) -> &str {
                "consumer"
            }
            fn inject(&self) -> Vec<String> {
                vec!["database".to_owned()]
            }
            fn apply(&self, ctx: &Context) -> Result<Effect, KernelError> {
                // 能跑到这里，就说明 database 一定在。
                let db = ctx
                    .resolve::<u64>("database")
                    .map_err(|_| KernelError::ServiceMissing("database 应当已就位"))?;
                assert_eq!(*db, 42);
                Ok(Effect::none())
            }
        }

        struct DbProvider;
        impl Plugin for DbProvider {
            fn name(&self) -> &str {
                "db-provider"
            }
            fn apply(&self, ctx: &Context) -> Result<Effect, KernelError> {
                Ok(ctx.provide_named("database", 42u64)?)
            }
        }

        let mut manager = PluginManager::new(Context::new());
        // 消费者先进场，提供者姗姗来迟。
        manager.load(Consumer).unwrap();
        assert_eq!(manager.pending().len(), 1);
        assert!(manager.active().is_empty());

        manager.load(DbProvider).unwrap();
        assert_eq!(manager.active(), ["consumer", "db-provider"]);
    }

    /// 服务被注销（不是插件被卸载）也要能把依赖方打回去：
    /// 热替换某个服务提供者时走的就是这条路径。
    #[test]
    fn losing_a_bare_service_also_unloads_the_dependent() {
        struct Consumer;
        impl Plugin for Consumer {
            fn name(&self) -> &str {
                "consumer"
            }
            fn inject(&self) -> Vec<String> {
                vec!["database".to_owned()]
            }
            fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
                Ok(Effect::none())
            }
        }

        let ctx = Context::new();
        let mut manager = PluginManager::new(ctx.clone());
        manager.load(Consumer).unwrap();

        let guard = ctx.provide_named("database", 1u64).unwrap();
        assert_eq!(manager.active(), ["consumer"]);

        // 服务消失（提供者被热替换时就是这条路径）。
        drop(guard);
        assert!(
            manager.active().is_empty(),
            "依赖的服务没了，consumer 该退回去"
        );
    }

    /// `apply` 失败只是它自己的 Failed 分支，不拖累别人。
    #[test]
    fn a_failing_apply_only_fails_that_one_plugin() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        manager.load(Failing).unwrap();
        assert_eq!(manager.active(), ["A"]);

        let failed = manager.failed();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "failing");
        assert!(failed[0].1.contains("apply 故意失败"));
    }

    #[test]
    fn duplicate_name_is_still_an_error() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        assert!(matches!(
            manager.load_shared(Named::new("A", &[], &log)),
            Err(KernelError::PluginAlreadyLoaded(_))
        ));

        let mut fresh = PluginManager::new(Context::new());
        assert!(matches!(
            fresh.load_all(vec![Named::new("A", &[], &log), Named::new("A", &[], &log)]),
            Err(KernelError::DuplicatePlugin(_))
        ));
    }

    /// 保险丝：就算依赖图构了环，reconcile 也必须能收住——谁都满足不了，
    /// 谁也起不来，循环很快跑完 budget 撒手。
    #[test]
    fn cyclic_dependencies_do_not_hang_reconciliation() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        let batch = vec![Named::new("A", &["B"], &log), Named::new("B", &["A"], &log)];
        let activated = manager.load_all(batch).unwrap();
        assert!(activated.is_empty(), "互相等对方，谁都起不来");
        assert!(manager.active().is_empty());
        assert_eq!(manager.pending().len(), 2);
    }

    #[test]
    fn unload_all_disposes_in_reverse_registration_order() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        manager.load_shared(Named::new("B", &["A"], &log)).unwrap();
        assert_eq!(manager.unload_all().unwrap(), ["B", "A"]);
        assert_eq!(*log.lock().unwrap(), ["B", "A"]);
    }
}
