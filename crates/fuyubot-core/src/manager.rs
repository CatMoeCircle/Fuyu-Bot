//! [`PluginManager`]：Fiber 生命周期、依赖注入与卸载回滚。
//!
//! 这一版把「**加载顺序**」换成了「**可用性**」（Cordis 的做法）：
//! 插件不必「先加载依赖再加载我」，而是声明它要哪些能力（[`Plugin::inject`]），
//! **依赖齐了才跑 `apply`**，在那之前它停在 Pending——那是合法状态，不是错误。
//!
//! 依赖是**持续跟踪**的：某个能力被注销了，依赖它的插件自动退回 Pending
//! （它已生效的注册一并回滚）；能力回来时再自动重新激活。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

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

    /// 软依赖是不是也到位了。**不影响激活**，只影响排序——见
    /// [`Plugin::soft_inject`](crate::Plugin::soft_inject)。
    fn soft_requirements_met(&self, ctx: &Context, fibers: &[Fiber]) -> bool {
        self.plugin
            .soft_inject()
            .iter()
            .all(|capability| is_available(ctx, fibers, capability))
    }

    /// 有没有硬依赖**注定永远拿不到**（提供它的 fiber 已经 `Failed`）。
    ///
    /// 返回 `Some(那个名字)` 就说明这条 fiber 该被级联跳过——继续 Pending
    /// 只会假装「还在等」，而它等的那个东西已经永远回不来了。
    ///
    /// ⚠️ **只对 `Failed` fiber 级联**。如果注入名没有任何 fiber 匹配
    /// （可能是未来出现的服务），不级联——那还是 PENDING 语义。
    fn permanently_missing(&self, ctx: &Context, fibers: &[Fiber]) -> Option<String> {
        self.plugin
            .inject()
            .into_iter()
            .find(|capability| is_permanently_missing(ctx, fibers, capability))
    }
}

/// 一个能力的名字有没有被满足。
fn is_available(ctx: &Context, fibers: &[Fiber], capability: &str) -> bool {
    ctx.has(capability)
        || fibers
            .iter()
            .any(|other| other.state.is_active() && other.name() == capability)
}

/// 这个能力是不是**注定永远拿不到**（提供它的 fiber 已经 `Failed`）。
///
/// 区分三种「拿不到」：
/// - 服务表里有 → `false`（已经满足了）；
/// - 有 Pending/Activating/Active 的同名 fiber → `false`（可能将来会满足）；
/// - 有 `Failed` 的同名 fiber → `true`（提供者挂了，永远回不来）；
/// - 谁都没有 → `false`（可能是未来出现的服务，保持 PENDING 语义）。
fn is_permanently_missing(ctx: &Context, fibers: &[Fiber], capability: &str) -> bool {
    if ctx.has(capability) {
        return false;
    }
    fibers
        .iter()
        .any(|other| other.name() == capability && matches!(other.state, State::Failed { .. }))
}

/// 把因依赖失败而注定拿不到的 Pending fiber **级联标记为 Failed**。
///
/// 递归到不动点：B 挂 → A（依赖 B）挂 → C（依赖 A）挂……一次调用搞定整条链。
///
/// 只处理**硬依赖**（[`Plugin::inject`]）：软依赖缺了不影响激活，不该级联。
fn cascade_failed(ctx: &Context, fibers: &mut [Fiber]) {
    loop {
        let mut changed = false;
        for index in 0..fibers.len() {
            if !matches!(fibers[index].state, State::Pending) {
                continue;
            }
            if let Some(capability) = fibers[index].permanently_missing(ctx, fibers) {
                fibers[index].state = State::Failed {
                    reason: format!("依赖 {capability} 加载失败，级联跳过"),
                };
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// 在注册任何 fiber 之前检测硬依赖环。发现就返回 [`KernelError::DependencyCycle`]。
///
/// 只看**硬依赖**（[`Plugin::inject`]）：软依赖成环只是排序上的死循环，不影响
/// 正确性——`reconcile_loop` 的 budget 兜底就够了，报错反而把合法的「互相软依赖」
/// 误杀。
///
/// 检测用 DFS + 递归栈。`n` 是插件数（通常 < 100），O(n²) 完全够用。
fn detect_cycle(plugins: &[Arc<dyn Plugin>]) -> Result<(), KernelError> {
    let n = plugins.len();
    // name → index，只认本批次里的插件名（注入的服务名不参与环检测）。
    let by_name: HashMap<&str, usize> = plugins
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name(), i))
        .collect();

    // 邻接表：i → [j] 表示 plugin i 的硬依赖指向 plugin j。
    let mut deps: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, plugin) in plugins.iter().enumerate() {
        for cap in plugin.inject() {
            if let Some(&j) = by_name.get(cap.as_str()) {
                deps[i].push(j);
            }
        }
    }

    // DFS 三色标记：0 = 未访问，1 = 在递归栈上，2 = 已完成。
    let mut visited = vec![0u8; n];
    let mut path = Vec::new();
    let names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();

    fn dfs(
        u: usize,
        deps: &[Vec<usize>],
        visited: &mut [u8],
        path: &mut Vec<usize>,
        names: &[&str],
    ) -> Result<(), String> {
        visited[u] = 1;
        path.push(u);
        for &v in &deps[u] {
            if visited[v] == 0 {
                if let Err(cycle) = dfs(v, deps, visited, path, names) {
                    return Err(cycle);
                }
            } else if visited[v] == 1 {
                // 找到环：从 path 里截出环的那段。
                let start = path.iter().position(|&x| x == v).unwrap();
                let cycle: Vec<&str> = path[start..].iter().map(|&i| names[i]).collect();
                return Err(cycle.join(" → "));
            }
        }
        path.pop();
        visited[u] = 2;
        Ok(())
    }

    for i in 0..n {
        if visited[i] == 0 {
            if let Err(cycle) = dfs(i, &deps, &mut visited, &mut path, &names) {
                return Err(KernelError::DependencyCycle { cycle });
            }
        }
    }
    Ok(())
}

/// 把当前 fiber 表拍成一份 [`Report`]。**加锁由调用方负责**（它可能已经持锁）。
fn snapshot(ctx: &Context, fibers: &[Fiber]) -> Report {
    let mut plugins = Vec::with_capacity(fibers.len());
    for fiber in fibers.iter() {
        let state = match &fiber.state {
            State::Pending => ReportState::Pending,
            State::Activating => ReportState::Activating,
            State::Active { .. } => ReportState::Active,
            State::Failed { .. } => ReportState::Failed,
        };
        // 只有「还没起来」的才值得报缺什么：活着的那些依赖自然是齐的。
        let missing = match state {
            ReportState::Pending => fiber.missing(ctx, fibers),
            _ => Vec::new(),
        };
        let reason = match &fiber.state {
            State::Failed { reason } => Some(reason.clone()),
            _ => None,
        };
        plugins.push(ReportEntry {
            name: fiber.name().to_owned(),
            state,
            missing,
            reason,
            // 激活序号：宿主按它排出「谁先起来的」，比登记顺序有意义。
            seq: fiber.seq,
        });
    }
    Report { plugins }
}

/// 一个**活的**报告来源，见 [`PluginManager::reporter`]。
///
/// 与 [`Report`]（一次性快照）的区别就是这一个：问几次算几次。
/// 它持**弱**引用，所以不会把管理器吊在那儿——管理器没了就报空表，
/// 这比 panic 或返回过期数据都好。
#[derive(Debug, Clone)]
pub struct PluginReporter {
    ctx: Context,
    fibers: Weak<Mutex<Vec<Fiber>>>,
}

impl PluginReporter {
    /// 现算一份。管理器已经没了就是空表（不是错误：调用它的多半是插件，
    /// 而插件可能比管理器活得久）。
    pub fn report(&self) -> Report {
        let Some(fibers) = self.fibers.upgrade() else {
            return Report {
                plugins: Vec::new(),
            };
        };
        let guard = fibers.lock().unwrap_or_else(|e| e.into_inner());
        snapshot(&self.ctx, &guard)
    }
}

/// [`PluginManager::report`] 的产物：一次加锁拿到的一致视图。
///
/// 故意不是 serde 类型：`fuyubot-core` 不引 serde（它连 Telegram 都不认识），
/// 序列化在宿主层（`fuyubot-loader`）做。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// 登记顺序。**不是**激活顺序——要那个看每项的 `seq`。
    pub plugins: Vec<ReportEntry>,
}

impl Report {
    /// 真在跑的插件名，按激活先后。
    pub fn active(&self) -> Vec<&str> {
        let mut live: Vec<(u64, &str)> = self
            .plugins
            .iter()
            .filter(|entry| entry.state == ReportState::Active)
            .map(|entry| (entry.seq.unwrap_or(0), entry.name.as_str()))
            .collect();
        live.sort_by_key(|(seq, _)| *seq);
        live.into_iter().map(|(_, name)| name).collect()
    }

    /// 还在等依赖的：`(插件名, 缺哪些能力)`。
    pub fn pending(&self) -> Vec<(&str, &[String])> {
        self.plugins
            .iter()
            .filter(|entry| entry.state == ReportState::Pending)
            .map(|entry| (entry.name.as_str(), entry.missing.as_slice()))
            .collect()
    }
}

/// 一个插件在快照里的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    pub name: String,
    pub state: ReportState,
    /// 只对 `Pending` 有意义：还缺哪些能力。
    pub missing: Vec<String>,
    /// 只对 `Failed` 有意义：`apply` 报的错。
    pub reason: Option<String>,
    /// 激活序号（没激活过是 `None`）。宿主按它排「谁先起来的」。
    pub seq: Option<u64>,
}

/// 对外暴露的状态名。**故意只有四态**：内核内部还有 `Activating` 这种瞬时态，
/// 但对外报它只会让人以为插件卡住了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    /// 依赖没齐，等着。**不是失败。**
    Pending,
    /// `apply` 正在跑。
    Activating,
    /// 活着。
    Active,
    /// `apply` 报错。
    Failed,
}

impl ReportState {
    /// 稳定的短名（进日志、进 JSON）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Activating => "activating",
            Self::Active => "active",
            Self::Failed => "failed",
        }
    }
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

    /// 一份**结构化**快照：谁登记了、谁真在跑、谁在等什么、谁挂了。
    ///
    /// 这是「插件问不到进程里加载了哪些」那个缺口的宿主侧答案：数据本来就有，
    /// 四个查询（[`loaded`](Self::loaded) / [`active`](Self::active) /
    /// [`pending`](Self::pending) / [`failed`](Self::failed)）各自答一半，
    /// 这里合成**一次加锁**的一致视图——分开调会在两次之间漏掉状态变化。
    ///
    /// 返回纯数据（不含 serde），序列化由宿主层做：`core` 不引 serde。
    ///
    /// ⚠️ 这是一份**快照**：拿到之后管理器再怎么变，它都不变。要给外部消费者
    /// （比如插件的 `fuyubot.host.plugins` 服务）用，请拿
    /// [`reporter`](Self::reporter) ——那个每次被问都现算，不会过期。
    pub fn report(&self) -> Report {
        snapshot(&self.ctx, &self.lock_fibers())
    }

    /// 一个**活的**报告来源：每次 [`PluginReporter::report`] 都现算。
    ///
    /// 这就是修掉「快照只在加载时刷一次」毛病的地方。之前宿主把手算的快照塞进
    /// 一个 `Mutex<String>`，指望调用方记得刷新——没人记得，于是运行期插件上下线
    /// 之后 `/plugins` 还在报旧状态（说三个在跑，其实退了两个）。
    ///
    /// 现在把**计算能力**交出去，而不是交结果：句柄持的是 `Arc<Mutex<Vec<Fiber>>>`
    /// 的弱引用 + `Context` 的克隆，问一次算一次，结构上不存在过期。
    pub fn reporter(&self) -> PluginReporter {
        PluginReporter {
            ctx: self.ctx.clone(),
            fibers: Arc::downgrade(&self.fibers),
        }
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
        // 环在注册前就报掉——不留半加载状态（Cordis 同样是启动期拒绝）。
        detect_cycle(&plugins)?;
        // 重名也在**改表之前**整批验完。旧写法边验边 push：第 N 个撞名时前 N-1 个
        // 已经进了表，却把 `Err` 交回给调用方——「失败 = 什么都没发生」不成立，
        // 而且那半批会在之后的 `reconcile` 里真的被激活。
        {
            let fibers = self.lock_fibers();
            let mut seen: HashSet<&str> = HashSet::with_capacity(plugins.len());
            for plugin in &plugins {
                let name = plugin.name();
                if fibers.iter().any(|fiber| fiber.name() == name) || !seen.insert(name) {
                    return Err(KernelError::DuplicatePlugin(name.to_owned()));
                }
            }
        }
        {
            let mut fibers = self.lock_fibers();
            for plugin in plugins {
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
        //
        // 级联跳过不在这里做——它由 `cascade_failed` 在 `Step::Activate`
        // 失败之后统一处理（递归到不动点），而不是每轮都扫一遍。
        let mut next = None;
        for (index, fiber) in guard.iter().enumerate() {
            if fiber.state.is_active() && !fiber.requirements_met(ctx, &guard) {
                next = Some((index, Step::Deactivate));
                break;
            }
        }
        if next.is_none() {
            // 软依赖排序偏好：**先激活软依赖也齐了的**，再激活只有硬依赖齐了的。
            // 这保证「A 软依赖 B」时 B 先于 A 激活（B 没来也行，只是排后面）。
            //
            // 两轮扫描，每轮都按注册顺序（= 文件名字典序，见 `plugin_files`），
            // 所以同层 tie-break 天然是确定的。
            for (index, fiber) in guard.iter().enumerate() {
                if matches!(fiber.state, State::Pending)
                    && fiber.requirements_met(ctx, &guard)
                    && fiber.soft_requirements_met(ctx, &guard)
                {
                    next = Some((index, Step::Activate));
                    break;
                }
            }
            if next.is_none() {
                for (index, fiber) in guard.iter().enumerate() {
                    if matches!(fiber.state, State::Pending) && fiber.requirements_met(ctx, &guard)
                    {
                        next = Some((index, Step::Activate));
                        break;
                    }
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
                // 依赖它的 fiber 注定拿不到 → 级联跳过（递归到不动点）。
                if matches!(guard[index].state, State::Failed { .. }) {
                    cascade_failed(ctx, &mut guard);
                }
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
        // 失败就是什么都没发生：旧写法边验边 push，撞名时前一个已经入库并会被激活，
        // 调用方却以为整批都没加载。
        assert_eq!(fresh.len(), 0, "重名失败不能留下半批插件");
        assert!(fresh.active().is_empty());
    }

    /// 环在启动前报错（`load_all` 入口的 `detect_cycle`），不再靠 budget 静默吸收。
    #[test]
    fn cyclic_dependencies_are_rejected_at_load_time() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        let batch = vec![Named::new("A", &["B"], &log), Named::new("B", &["A"], &log)];
        let result = manager.load_all(batch);
        assert!(
            matches!(result, Err(KernelError::DependencyCycle { .. })),
            "{result:?}"
        );
        assert_eq!(manager.len(), 0, "不留半加载状态");
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

    /// 报告要一次给出**四种状态**的全貌，并且只在有意义的字段上带细节。
    #[test]
    fn report_covers_every_state_without_noise() {
        struct Provider;
        impl Plugin for Provider {
            fn name(&self) -> &str {
                "provider"
            }
            fn apply(&self, ctx: &Context) -> Result<Effect, KernelError> {
                ctx.provide_named("database", 1_u8)
            }
        }

        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        manager.load(Provider).unwrap();
        manager
            .load_shared(Named::new("waiter", &["nope"], &log))
            .unwrap();
        manager.load(Failing).unwrap();

        let report = manager.report();
        assert_eq!(report.plugins.len(), 3);

        // 活着的：有 seq、没有 missing、没有 reason。
        let provider = report
            .plugins
            .iter()
            .find(|entry| entry.name == "provider")
            .expect("provider 在报告里");
        assert_eq!(provider.state, ReportState::Active);
        assert!(provider.seq.is_some(), "激活过就该有 seq");
        assert!(provider.missing.is_empty());
        assert!(provider.reason.is_none());

        // 等着的：报出缺什么，且**没有** seq。
        let pending = report.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "waiter");
        assert_eq!(pending[0].1, ["nope".to_owned()].as_slice());
        let waiter = report
            .plugins
            .iter()
            .find(|entry| entry.name == "waiter")
            .expect("waiter 在报告里");
        assert!(waiter.seq.is_none(), "没激活过就不该有 seq");

        // 失败的：带原因，不报 missing（它压根不是因为缺依赖躺着）。
        let failing = report
            .plugins
            .iter()
            .find(|entry| entry.name == "failing")
            .expect("failing 在报告里");
        assert_eq!(failing.state, ReportState::Failed);
        assert!(failing.reason.is_some());
        assert!(failing.missing.is_empty());

        assert_eq!(report.active(), vec!["provider"]);
    }

    /// 空管理器也要能出报告（`status` 在「一个插件都没加载」时照样调）。
    #[test]
    fn an_empty_manager_reports_an_empty_snapshot() {
        let manager = PluginManager::new(Context::new());
        let report = manager.report();
        assert!(report.plugins.is_empty());
        assert!(report.active().is_empty());
        assert!(report.pending().is_empty());
    }

    /// `reporter()` 是**活的**：管理器状态一变，同一个句柄问出来就不一样。
    ///
    /// 这条守的是「快照被人存下来忘了刷」那个 bug——旧实现把算好的 `Report`
    /// 序列化进一个 `Mutex<String>`，运行期插件上下线之后它一直是旧的。
    #[test]
    fn a_reporter_tracks_state_instead_of_freezing_it() {
        let log = Arc::new(StdMutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        let reporter = manager.reporter();

        assert!(reporter.report().plugins.is_empty(), "一开始没人");

        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        assert_eq!(
            reporter.report().active(),
            vec!["A"],
            "加载后同一个句柄就该看到"
        );

        manager.unload_all().unwrap();
        assert!(
            reporter.report().plugins.is_empty(),
            "卸完之后它必须报空，而不是留着旧的那份"
        );
    }

    /// 管理器没了之后 reporter 报空表，**不 panic**：它持有的是弱引用
    /// （插件可能比管理器活得久，那种时候「没人了」是正确的答案）。
    #[test]
    fn a_reporter_outliving_its_manager_reports_nothing() {
        let reporter = {
            let manager = PluginManager::new(Context::new());
            manager.reporter()
        };
        assert!(reporter.report().plugins.is_empty());
    }
}

// ── 硬/软依赖 + 级联跳过 + 环检测 ──────────────────────────────────

/// 软依赖不影响激活，只影响排序：两者都就绪时先激活被依赖的那个。
#[test]
fn soft_inject_orders_activation_but_never_blocks_it() {
    struct Named {
        name: &'static str,
        soft: Vec<String>,
    }
    impl Plugin for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn soft_inject(&self) -> Vec<String> {
            self.soft.clone()
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Ok(Effect::none())
        }
    }

    // B 软依赖 A：两者都就绪时 A 先激活。
    let mut manager = PluginManager::new(Context::new());
    manager
        .load_all(vec![
            Arc::new(Named {
                name: "b",
                soft: vec!["a".to_owned()],
            }),
            Arc::new(Named {
                name: "a",
                soft: vec![],
            }),
        ])
        .unwrap();
    assert_eq!(manager.active(), ["a", "b"], "A 应先于 B");

    // 软依赖缺失照样加载：A 不在列表里，B 不该等它。
    let mut manager = PluginManager::new(Context::new());
    manager
        .load_all(vec![Arc::new(Named {
            name: "b",
            soft: vec!["never-exists".to_owned()],
        })])
        .unwrap();
    assert_eq!(manager.active(), ["b"], "软依赖缺失不该阻塞");
}

/// 级联跳过：B 加载失败 → 依赖 B 的 A 也不该起；递归传播到 C。
#[test]
fn a_failed_provider_cascades_to_its_dependents() {
    struct Named {
        name: &'static str,
        injects: Vec<String>,
    }
    impl Plugin for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn inject(&self) -> Vec<String> {
            self.injects.clone()
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Ok(Effect::none())
        }
    }
    struct Boom;
    impl Plugin for Boom {
        fn name(&self) -> &str {
            "b"
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Err(KernelError::ServiceMissing("boom"))
        }
    }

    let mut manager = PluginManager::new(Context::new());
    manager
        .load_all(vec![
            Arc::new(Named {
                name: "c",
                injects: vec!["a".to_owned()],
            }),
            Arc::new(Named {
                name: "a",
                injects: vec!["b".to_owned()],
            }),
            Arc::new(Boom),
        ])
        .unwrap();

    assert!(manager.active().is_empty(), "谁都该起不来");
    let failed = manager.failed();
    assert_eq!(failed.len(), 3, "B 挂 → A 挂 → C 挂，递归级联");
    for (name, reason) in &failed {
        if name == "b" {
            assert!(reason.contains("boom"), "{reason}");
        } else {
            assert!(reason.contains("级联跳过"), "{name}: {reason}");
        }
    }
    // Pending 应该是空的——级联之后没人还在「假装等待」。
    assert!(manager.pending().is_empty());
}

/// 能力名（不是插件名）没有 Failed 提供者时**不级联**——那是 PENDING 语义。
#[test]
fn a_service_name_without_a_failed_provider_stays_pending_not_cascaded() {
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

    let mut manager = PluginManager::new(Context::new());
    manager.load(Consumer).unwrap();
    // "database" 没有任何 fiber 提供，也不是 Failed——它可能是未来出现的服务。
    assert_eq!(manager.pending().len(), 1);
    assert!(manager.failed().is_empty(), "不该级联——这仍是 PENDING");
}

/// 环在启动前报错，不留半加载状态。
#[test]
fn a_dependency_cycle_is_reported_before_anything_loads() {
    struct Named {
        name: &'static str,
        injects: Vec<String>,
    }
    impl Plugin for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn inject(&self) -> Vec<String> {
            self.injects.clone()
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Ok(Effect::none())
        }
    }

    let mut manager = PluginManager::new(Context::new());
    let result = manager.load_all(vec![
        Arc::new(Named {
            name: "a",
            injects: vec!["b".to_owned()],
        }),
        Arc::new(Named {
            name: "b",
            injects: vec!["a".to_owned()],
        }),
    ]);
    assert!(
        matches!(result, Err(KernelError::DependencyCycle { .. })),
        "{result:?}"
    );
    assert_eq!(manager.len(), 0, "不留半加载状态");
}

/// 软依赖成环**不报错**——那只是排序上的死循环，不影响正确性。
#[test]
fn a_soft_dependency_cycle_is_not_an_error() {
    struct Named {
        name: &'static str,
        soft: Vec<String>,
    }
    impl Plugin for Named {
        fn name(&self) -> &str {
            self.name
        }
        fn soft_inject(&self) -> Vec<String> {
            self.soft.clone()
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Ok(Effect::none())
        }
    }

    let mut manager = PluginManager::new(Context::new());
    let result = manager.load_all(vec![
        Arc::new(Named {
            name: "a",
            soft: vec!["b".to_owned()],
        }),
        Arc::new(Named {
            name: "b",
            soft: vec!["a".to_owned()],
        }),
    ]);
    assert!(result.is_ok(), "软依赖成环不该报错：{result:?}");
    assert_eq!(manager.active().len(), 2);
}

/// 自环也算环。
#[test]
fn a_self_loop_is_a_cycle() {
    struct SelfDep;
    impl Plugin for SelfDep {
        fn name(&self) -> &str {
            "self"
        }
        fn inject(&self) -> Vec<String> {
            vec!["self".to_owned()]
        }
        fn apply(&self, _ctx: &Context) -> Result<Effect, KernelError> {
            Ok(Effect::none())
        }
    }

    let mut manager = PluginManager::new(Context::new());
    let result = manager.load_all(vec![Arc::new(SelfDep)]);
    assert!(
        matches!(result, Err(KernelError::DependencyCycle { .. })),
        "{result:?}"
    );
}
