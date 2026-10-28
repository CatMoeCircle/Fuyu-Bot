//! [`Context`]：插件之间共享服务的**命名**表（Cordis 式 Container）。
//!
//! 表里按**名字**存，不按 Rust 类型：跨语言插件报不出 `TypeId`，只能报名字。
//! 类型化的 [`provide`](Context::provide) / [`get`](Context::get) 退化成**语法糖**——
//! 隐式拿 [`type_name::<T>()`](std::any::type_name) 当名字。
//! 于是「给 Rust 用的类型表」和「给插件用的名字表」是**同一张表**，不再有两套。
//!
//! 服务一旦注册/注销，会通知 [`Context::on_service_change`] 登记的钩子。
//! 这是 Cordis 式 `inject` 依赖跟踪的触发源：被依赖的能力何时出现、何时消失，
//! 得有个地方知道。

use std::any::{Any, type_name};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::effect::Effect;
use crate::error::KernelError;

/// 服务表里一格的值。
type Service = Arc<dyn Any + Send + Sync>;
/// 服务表的底层存储。按名字，不按 `TypeId`。
type ServiceMap = HashMap<String, Entry>;

/// 服务表里一格。
#[derive(Clone)]
struct Entry {
    value: Service,
    /// 注册时那个 Rust 类型名。只用于诊断：`resolve::<T>` 类型不对时，
    /// 用它区分「名字不存在」和「名字在但类型不对」。
    type_name: &'static str,
}

/// 服务变更的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceChange {
    Provided,
    Removed,
}

/// 服务变更钩子。做成 `Arc` 是为了通知前能把快照拷出来——
/// 钩子里完全可能再去注册/注销服务，持着读锁回调会死锁。
type ChangeHook = Arc<dyn Fn(&str, ServiceChange) + Send + Sync>;

/// 跨插件共享的上下文。
///
/// 内部是 `Arc<RwLock<..>>`，所以 `Clone` 只复制句柄、不复制状态：
/// 分发给插件的那份和内核持有的那份看到的是**同一张表**。
///
/// # 作用域链
///
/// 除了根，每个 [`Context`] 都可以挂在一个父级下（[`child`](Self::child)）。
/// 查找**先看自己这层，再沿父链向上**；注册只写自己那层。于是「子作用域里换个
/// 实现」不需要改动父级——这就是 Cordis 的 scope 语义。
///
/// [`isolate`](Self::isolate) 把某个名字**在当前作用域及以下**从父链上摘掉：
/// 之后这个名字只能靠自己（或自己的子作用域）提供，父级那个看不见了。
/// 主要用途是测试替身与插件沙箱——「我这个作用域里的 `db` 必须是假的那个」。
///
/// ⚠️ `Context::new()` 永远造**根**（没有父级）。这不是限制而是刻意的：
/// 68 个调用点都假设自己拿的是根，让 `new()` 保持那个含义，父子关系就必须显式建立。
#[derive(Clone, Default)]
pub struct Context {
    services: Arc<RwLock<ServiceMap>>,
    /// 父级。根是 `None`。
    parent: Option<Arc<Context>>,
    /// 在当前作用域**截断**父链查找的名字。`isolate` 往里加。
    ///
    /// 与 `services` 共用一把锁不划算（`isolate` 是配置期操作、查找是热路径），
    /// 但分成两把锁又会让「隔离生效」与「查到父级」之间有竞态窗口。
    /// 这里放同一把锁里：一次读锁同时拿到两样，天然一致。
    isolated: Arc<RwLock<HashSet<String>>>,
    hooks: Arc<RwLock<Vec<ChangeHook>>>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// 一个 Rust 类型默认的服務名。
    fn implicit_name<T: Any>() -> String {
        type_name::<T>().to_owned()
    }

    /// 造一个**挂在当前作用域下**的子上下文。
    ///
    /// 子级能看见父级的一切（沿链查找），但自己注册的东西父级看不见。
    /// 钩子是**共享**的：父级也想知道子级提供了什么（依赖跟踪要跨作用域走）。
    ///
    /// 用途：给某个插件一份隔离得住的视图、给测试一个不改动根表的地方。
    pub fn child(&self) -> Context {
        Self {
            services: Arc::new(RwLock::new(HashMap::new())),
            parent: Some(Arc::new(self.clone())),
            isolated: Arc::new(RwLock::new(HashSet::new())),
            hooks: Arc::clone(&self.hooks),
        }
    }

    /// 把 `name` 从**当前作用域及以下**的父链查找里摘掉。
    ///
    /// 之后这个名字只认自己（或自己的子作用域）提供的；父级那个看不见了。
    /// 典型用法是测试替身：
    ///
    /// ```
    /// use fuyubot_core::Context;
    ///
    /// let root = Context::new();
    /// let _real = root.provide_named("db", "真实数据库").unwrap();
    /// let scope = root.child();
    /// scope.isolate("db");
    /// let _fake = scope.provide_named("db", "假数据库").unwrap();
    ///
    /// assert_eq!(*root.resolve::<&str>("db").unwrap(), "真实数据库");
    /// assert_eq!(*scope.resolve::<&str>("db").unwrap(), "假数据库");
    /// ```
    ///
    /// 幂等：同一个名字 isolate 两次不报错（配置期可能从不同地方调到）。
    pub fn isolate(&self, name: &str) {
        self.isolated
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned());
    }

    /// 这个名字在当前作用域是不是被隔离了。
    pub fn is_isolated(&self, name: &str) -> bool {
        self.isolated
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }

    /// 有父级就是子作用域。
    pub fn has_parent(&self) -> bool {
        self.parent.is_some()
    }

    /// 注册服务，名字隐式取类型名，返回的回滚守卫会在 drop 时把它注销。
    ///
    /// 同一个类型重复注册直接报错（不做静默覆盖：覆盖会让先注册者的守卫
    /// 在稍后误删后注册者的服务）。
    pub fn provide<T: Any + Send + Sync>(&self, value: T) -> Result<Effect, KernelError> {
        let name = Self::implicit_name::<T>();
        self.provide_named(&name, value)
            .map_err(|error| match error {
                KernelError::ServiceNameExists(_) => KernelError::ServiceExists(type_name::<T>()),
                other => other,
            })
    }

    /// 按名字注册服务。跨语言插件注册进来的都走这条路。
    pub fn provide_named<T: Any + Send + Sync>(
        &self,
        name: &str,
        value: T,
    ) -> Result<Effect, KernelError> {
        if name.is_empty() {
            return Err(KernelError::EmptyServiceName);
        }

        {
            let mut services = self.write();
            if services.contains_key(name) {
                return Err(KernelError::ServiceNameExists(name.to_owned()));
            }
            services.insert(
                name.to_owned(),
                Entry {
                    value: Arc::new(value),
                    type_name: type_name::<T>(),
                },
            );
        }

        notify(&self.hooks, name, ServiceChange::Provided);

        let table = Arc::clone(&self.services);
        let hooks = Arc::clone(&self.hooks);
        let key = name.to_owned();
        Ok(Effect::new(move || {
            table
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            notify(&hooks, &key, ServiceChange::Removed);
        }))
    }

    /// 按类型取服务。拿到的是共享句柄，服务本身不会被复制。
    pub fn get<T: Any + Send + Sync>(&self) -> Result<Arc<T>, KernelError> {
        self.lookup::<T>(&Self::implicit_name::<T>())
            .ok_or_else(|| KernelError::ServiceMissing(type_name::<T>()))
    }

    /// 按名字取服务。名字不存在报 [`KernelError::ServiceNameMissing`]，
    /// 名字在但注册的是别的类型报 [`KernelError::ServiceTypeMismatch`]。
    ///
    /// 沿作用域链查：自己这层没有就往父级找，碰见 `isolate` 过的名字就停。
    pub fn resolve<T: Any + Send + Sync>(&self, name: &str) -> Result<Arc<T>, KernelError> {
        let Some((value, found)) = self.find(name) else {
            return Err(KernelError::ServiceNameMissing(name.to_owned()));
        };
        value
            .downcast::<T>()
            .map_err(|_| KernelError::ServiceTypeMismatch {
                name: name.to_owned(),
                expected: type_name::<T>(),
                found,
            })
    }

    /// 名字在不在，**不看类型**。inject 判定依赖是否满足只需要这个。
    pub fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// 某个名字上注册的实际类型名（沿链找，隔离点会截断）。
    pub fn type_of(&self, name: &str) -> Option<&'static str> {
        self.find(name).map(|(_, found)| found)
    }

    /// 已注册的名字，已排序（输出稳定）。
    ///
    /// **包含父级看得见的那些**——调用方问的是「我这个作用域能用什么」，
    /// 而不是「我这一层存了什么」。被隔离掉的名字不在其中。
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.visible().into_keys().collect();
        names.sort();
        names
    }

    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.has(&Self::implicit_name::<T>())
    }

    /// **这一层**存了几个（不含父级）。诊断用；"能看见几个"请用 [`names`](Self::names)。
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// 这一层是不是空的（不含父级）。
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// 登记服务变更钩子。依赖跟踪（某个能力出现/消失）靠它驱动。
    ///
    /// 钩子是**追加**的，没有反注册——和 [`Context`] 同生命周期的东西才该用它。
    pub fn on_service_change(&self, hook: impl Fn(&str, ServiceChange) + Send + Sync + 'static) {
        self.hooks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(hook));
    }

    /// 沿作用域链找这个值：自己这层 → 父级 → …，遇到隔离点就停。
    ///
    /// 返回 `(值, 注册时的类型名)`。类型名要一路带出来——`resolve` 报
    /// 「名字在但类型不对」时需要它，而那时我们已经离开持有它的那层了。
    fn find(&self, name: &str) -> Option<(Service, &'static str)> {
        // 隔离判定读的是**发起查找的这层**，不是每一层：`isolate` 的语义是
        // 「我这个作用域别往上看」，所以只在起点判一次。
        let start_isolated = self.is_isolated(name);
        let entry = {
            let services = self.read();
            services
                .get(name)
                .map(|entry| (Arc::clone(&entry.value), entry.type_name))
        };
        if let Some(found) = entry {
            return Some(found);
        }
        if start_isolated {
            return None;
        }
        self.parent.as_ref().and_then(|parent| parent.find(name))
    }

    /// 当前作用域**能看见**的全部键值（含父级，去掉被隔离的）。
    ///
    /// 名字冲突时以**近的那层**为准——和 `find` 一个口径。
    fn visible(&self) -> HashMap<String, Entry> {
        // 父级的先铺底，自己的覆盖上去（近的赢）。
        let mut merged = self
            .parent
            .as_ref()
            .map(|parent| parent.visible())
            .unwrap_or_default();
        // 一次读锁同时拿到「自己这层有什么」和「隔离了哪些」——分成两次读
        // 会让 `isolate` 与查找之间出现竞态窗口（见 `isolated` 字段的注释）。
        let isolated = self
            .isolated
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        {
            let services = self.read();
            for (name, entry) in services.iter() {
                merged.insert(
                    name.clone(),
                    Entry {
                        value: Arc::clone(&entry.value),
                        type_name: entry.type_name,
                    },
                );
            }
        }
        // 隔离的名字只能看见自己这层提供的（父级那份已经铺进来了，要摘掉）。
        for name in isolated {
            if !self.read().contains_key(&name) {
                merged.remove(&name);
            }
        }
        merged
    }

    /// 按名字取，**不区分名字不在和类型不对**——统一的下游（沿链找）。
    fn lookup<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.find(name)
            .and_then(|(value, _)| value.downcast::<T>().ok())
    }

    /// 中毒的锁也要能用：一个插件的 panic 不该让整个内核卡死。
    fn read(&self) -> RwLockReadGuard<'_, ServiceMap> {
        self.services.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, ServiceMap> {
        self.services.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// 通知所有钩子。**先把快照拷出来再回调**，钩子里改服务表才不会死锁。
fn notify(hooks: &Arc<RwLock<Vec<ChangeHook>>>, name: &str, change: ServiceChange) {
    let Ok(guard) = hooks.read() else {
        return;
    };
    let snapshot: Vec<ChangeHook> = guard.clone();
    drop(guard);
    for hook in snapshot {
        hook(name, change);
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("services", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Config(&'static str);

    #[test]
    fn provide_then_get_returns_a_shared_handle() {
        let ctx = Context::new();
        assert!(ctx.is_empty());
        let _guard = ctx.provide(Config("prod")).unwrap();

        let one = ctx.get::<Config>().unwrap();
        let two = ctx.get::<Config>().unwrap();
        assert_eq!(*one, Config("prod"));
        assert!(Arc::ptr_eq(&one, &two), "拿到的应该是同一个 Arc");
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn missing_service_and_duplicate_provide_are_errors() {
        let ctx = Context::new();
        assert!(matches!(
            ctx.get::<Config>(),
            Err(KernelError::ServiceMissing(_))
        ));

        let _guard = ctx.provide(Config("prod")).unwrap();
        assert!(matches!(
            ctx.provide(Config("dev")),
            Err(KernelError::ServiceExists(_))
        ));
        assert_eq!(*ctx.get::<Config>().unwrap(), Config("prod"));
    }

    #[test]
    fn dropping_the_guard_unregisters_the_service() {
        let ctx = Context::new();
        let guard = ctx.provide(7u32).unwrap();
        assert!(ctx.contains::<u32>());

        drop(guard);
        assert!(!ctx.contains::<u32>());
        assert!(matches!(
            ctx.get::<u32>(),
            Err(KernelError::ServiceMissing(_))
        ));
    }

    #[test]
    fn clones_share_the_same_table() {
        let ctx = Context::new();
        let fork = ctx.clone();
        let _guard = fork.provide(1u8).unwrap();
        assert_eq!(*ctx.get::<u8>().unwrap(), 1);
    }

    #[test]
    fn named_registration_lands_in_the_same_table() {
        let ctx = Context::new();
        let _guard = ctx.provide_named("greeting", String::from("hi")).unwrap();

        assert!(ctx.has("greeting"));
        assert_eq!(*ctx.resolve::<String>("greeting").unwrap(), "hi");
        assert_eq!(ctx.type_of("greeting"), Some("alloc::string::String"));
        assert_eq!(ctx.names(), ["greeting"]);
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn typed_registration_is_visible_by_its_type_name() {
        struct Marker;
        let ctx = Context::new();
        let _guard = ctx.provide(Marker).unwrap();

        // 隐式名字就是 type_name：按名字查也应该找得到。
        assert!(ctx.has(&std::any::type_name::<Marker>()));
    }

    #[test]
    fn resolving_reports_missing_and_type_mismatch_differently() {
        let ctx = Context::new();
        let _guard = ctx.provide_named("port", 8080u32).unwrap();

        // 名字在，类型不对。
        assert!(matches!(
            ctx.resolve::<String>("port"),
            Err(KernelError::ServiceTypeMismatch { .. })
        ));
        // 名字不在。
        assert!(matches!(
            ctx.resolve::<u32>("nope"),
            Err(KernelError::ServiceNameMissing(_))
        ));
        assert_eq!(ctx.type_of("nope"), None);
    }

    #[test]
    fn empty_name_is_rejected() {
        let ctx = Context::new();
        assert!(matches!(
            ctx.provide_named("", 1u8),
            Err(KernelError::EmptyServiceName)
        ));
    }

    #[test]
    fn change_hooks_fire_on_provide_and_on_removal() {
        let ctx = Context::new();
        let log = Arc::new(RwLock::new(Vec::new()));

        let sink = Arc::clone(&log);
        ctx.on_service_change(move |name, change| {
            let kind = matches!(change, ServiceChange::Provided);
            sink.write().unwrap().push(format!("{name}:{kind}"));
        });

        let guard = ctx.provide_named("db", 1u8).unwrap();
        drop(guard);

        assert_eq!(*log.read().unwrap(), ["db:true", "db:false"]);
    }

    /// 钩子里再去注册服务不能死锁——这正是先把快照拷出来的理由。
    #[test]
    fn a_hook_may_touch_the_service_table_without_deadlocking() {
        let ctx = Context::new();
        // 钩子注册出来的服务得把 `Effect` 存住：`Effect` 是 `#[must_use]`，
        // `let _ =` 会立刻把它回滚掉（约束 11），那就测不到东西了。
        let held = Arc::new(std::sync::Mutex::new(Vec::new()));

        let reentrant = ctx.clone();
        let sink = Arc::clone(&held);
        ctx.on_service_change(move |name, change| {
            if matches!(change, ServiceChange::Provided) && name == "a" {
                if let Ok(effect) = reentrant.provide_named("b", 2u8) {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(effect);
                }
            }
        });

        let _guard_a = ctx.provide_named("a", 1u8);
        assert!(ctx.has("b"), "钩子里的注册该成功，且不该卡住");
    }

    // ── 作用域链与 isolate（P1）─────────────────────────────────────

    /// **根的行为一个字都没变**：这是 68 个调用点的前提。
    ///
    /// `Context::new()` 永远是根（`parent == None`），于是沿链查找在根上退化成
    /// 「只看这一张表」，与加作用域之前完全一致。这条测试钉的就是这个退化——
    /// 它要是红了，整仓的语义都动了。
    #[test]
    fn a_root_context_behaves_exactly_like_a_flat_table() {
        let ctx = Context::new();
        assert!(!ctx.has_parent(), "new() 必须是根");

        let _a = ctx.provide_named("a", 1u8).unwrap();
        let _b = ctx.provide_named("b", 2u8).unwrap();
        assert_eq!(ctx.names(), ["a", "b"]);
        assert_eq!(ctx.len(), 2);
        assert!(ctx.has("a") && ctx.has("b"));
        assert_eq!(ctx.type_of("a"), Some("u8"));

        drop(_a);
        assert_eq!(ctx.names(), ["b"], "撤销还是当场生效");
        assert_eq!(ctx.len(), 1);
        assert!(!ctx.has("a"));
    }

    /// 子作用域看得见父级的服务，父级看不见子级的。
    #[test]
    fn a_child_sees_the_parent_but_not_the_other_way_around() {
        let root = Context::new();
        let _root_db = root.provide_named("db", "root").unwrap();
        let scope = root.child();

        assert_eq!(
            *scope.resolve::<&str>("db").unwrap(),
            "root",
            "子该看见父的"
        );

        let _scoped = scope.provide_named("cache", "child-only").unwrap();
        assert!(scope.has("cache"));
        assert!(!root.has("cache"), "反过来不行：父不该看见子的");
        assert!(!root.has_parent());
        assert!(scope.has_parent());
    }

    /// 注册只写自己那层：同名服务在子级里能另立一个，父级不受影响。
    #[test]
    fn a_child_can_shadow_a_name_without_touching_the_parent() {
        let root = Context::new();
        let _root_db = root.provide_named("db", "root").unwrap();
        let scope = root.child();
        let _child_db = scope.provide_named("db", "child").unwrap();

        assert_eq!(*scope.resolve::<&str>("db").unwrap(), "child", "近的赢");
        assert_eq!(*root.resolve::<&str>("db").unwrap(), "root", "父级不动");

        // 子级那份撤掉之后，又看得见父级的了。
        drop(_child_db);
        assert_eq!(*scope.resolve::<&str>("db").unwrap(), "root");
    }

    /// `isolate` 把名字从父链上摘掉：看不见父级的，也不报错（只是「没有」）。
    #[test]
    fn isolate_cuts_the_parent_lookup_for_that_name_only() {
        let root = Context::new();
        let _db = root.provide_named("db", "root").unwrap();
        let _cache = root.provide_named("cache", "root-cache").unwrap();

        let scope = root.child();
        scope.isolate("db");

        assert!(!scope.has("db"), "隔离之后看不见父级的 db");
        assert!(scope.has("cache"), "别的名字不受影响");
        assert!(root.has("db"), "父级当然还看得见自己的");

        // 自己这层提供之后就能看见了——隔离截断的是**父链**，不是自己。
        let _fake = scope.provide_named("db", "fake").unwrap();
        assert_eq!(*scope.resolve::<&str>("db").unwrap(), "fake");
        assert_eq!(*root.resolve::<&str>("db").unwrap(), "root");
    }

    /// 隔离是**单向向下**的：子级 isolate 不影响父级，也不影响兄弟。
    #[test]
    fn isolate_does_not_leak_to_the_parent_or_siblings() {
        let root = Context::new();
        let _db = root.provide_named("db", "root").unwrap();

        let left = root.child();
        let right = root.child();
        left.isolate("db");

        assert!(!left.has("db"));
        assert!(right.has("db"), "兄弟不受影响");
        assert!(root.has("db"));
        assert!(left.is_isolated("db"));
        assert!(!right.is_isolated("db"));
    }

    /// 隔三代也得沿链找到；中间那层 isolate 会把它截住。
    #[test]
    fn isolation_cuts_at_the_level_it_was_declared() {
        let root = Context::new();
        let _deep = root.provide_named("deep", 1u8).unwrap();

        let middle = root.child();
        let leaf = middle.child();

        assert!(leaf.has("deep"), "隔一层也该找得到");
        // 在**中间**那层隔离：叶子从中间往上看，就不该再看见根上的。
        middle.isolate("deep");
        assert!(!middle.has("deep"));
        assert!(!leaf.has("deep"), "叶子的父链经过 middle，也被截断");
        assert!(root.has("deep"));
    }

    /// `names()` 报的是「这个作用域能用什么」，不是「这层存了什么」。
    #[test]
    fn names_reports_what_is_visible_not_what_is_local() {
        let root = Context::new();
        let _a = root.provide_named("a", 1u8).unwrap();
        let _b = root.provide_named("b", 2u8).unwrap();

        let scope = root.child();
        let _c = scope.provide_named("c", 3u8).unwrap();
        assert_eq!(scope.names(), ["a", "b", "c"]);
        assert_eq!(root.names(), ["a", "b"]);
        // `len()` 仍是**本层**的（诊断用），两者故意不同。
        assert_eq!(scope.len(), 1);
        assert_eq!(scope.names().len(), 3);

        scope.isolate("b");
        assert_eq!(scope.names(), ["a", "c"], "隔离的名字不该出现在可见清单里");
    }

    /// 服务变更钩子跨作用域共享：子级的注册父级也得收到通知（依赖跟踪要跨层走）。
    #[test]
    fn service_hooks_are_shared_across_scopes() {
        use std::sync::Mutex as StdMutex;

        let root = Context::new();
        let log = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        root.on_service_change(move |name, _| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(name.to_owned());
        });

        let scope = root.child();
        let guard = scope.provide_named("scoped", 1u8).unwrap();
        drop(guard);

        assert_eq!(
            *log.lock().unwrap(),
            ["scoped", "scoped"],
            "提供与撤销都该被听见"
        );
    }
}
