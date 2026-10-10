//! [`PluginManager`]：依赖解析、加载顺序与卸载回滚。

use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Context;
use crate::effect::Effect;
use crate::error::KernelError;
use crate::plugin::Plugin;

/// 一个已加载的插件。
struct Loaded {
    /// 用 `Arc` 而不是 `Box`，`reload` 才能先摘下来再装回去。
    plugin: Arc<dyn Plugin>,
    /// `apply` 交回来的回滚守卫；卸载时 drop 它。
    effect: Effect,
}

/// 插件管理器。
///
/// 加载顺序由依赖决定（拓扑序），卸载按加载顺序倒着来。
/// 所有注册都落在它持有的 [`Context`] 上。
pub struct PluginManager {
    ctx: Context,
    /// 按加载顺序排列。
    loaded: Vec<(String, Loaded)>,
}

impl PluginManager {
    pub fn new(ctx: Context) -> Self {
        Self {
            ctx,
            loaded: Vec::new(),
        }
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.iter().any(|(n, _)| n == name)
    }

    /// 已加载的插件名，按加载顺序。
    pub fn loaded(&self) -> Vec<&str> {
        self.loaded.iter().map(|(n, _)| n.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.loaded.len()
    }

    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// 加载单个插件。它的依赖必须已经加载。
    pub fn load(&mut self, plugin: impl Plugin + 'static) -> Result<String, KernelError> {
        self.load_shared(Arc::new(plugin))
    }

    /// 同上，但对已经是 `Arc` 的插件（`load_all` / `reload` 用）。
    pub fn load_shared(&mut self, plugin: Arc<dyn Plugin>) -> Result<String, KernelError> {
        let name = plugin.name().to_owned();
        if self.is_loaded(&name) {
            return Err(KernelError::PluginAlreadyLoaded(name));
        }
        for dependency in plugin.dependencies() {
            if !self.is_loaded(&dependency) {
                return Err(KernelError::MissingDependency {
                    plugin: name,
                    dependency,
                });
            }
        }

        let effect = plugin.apply(&self.ctx)?;
        self.loaded.push((name.clone(), Loaded { plugin, effect }));
        Ok(name)
    }

    /// 按依赖顺序加载一批插件，返回实际加载顺序。
    ///
    /// 中途失败会把本次已经加载的卸载掉，不留半加载状态。
    pub fn load_all(&mut self, plugins: Vec<Arc<dyn Plugin>>) -> Result<Vec<String>, KernelError> {
        let order = self.resolve_order(&plugins)?;
        let mut slots: Vec<Option<Arc<dyn Plugin>>> = plugins.into_iter().map(Some).collect();
        let mut loaded = Vec::with_capacity(order.len());

        for index in order {
            let plugin = slots[index].take().expect("拓扑序里每个下标只出现一次");
            match self.load_shared(plugin) {
                Ok(name) => loaded.push(name),
                Err(error) => {
                    for name in loaded.iter().rev() {
                        let _ = self.unload(name);
                    }
                    return Err(error);
                }
            }
        }
        Ok(loaded)
    }

    /// 卸载一个插件：先 `dispose`，再 drop 守卫触发回滚。
    ///
    /// 还有别的插件依赖它时拒绝卸载，而不是把依赖者一起带走。
    pub fn unload(&mut self, name: &str) -> Result<(), KernelError> {
        let position = self
            .loaded
            .iter()
            .position(|(n, _)| n == name)
            .ok_or_else(|| KernelError::PluginNotLoaded(name.to_owned()))?;

        let dependents: Vec<String> = self
            .loaded
            .iter()
            .filter(|(_, loaded)| loaded.plugin.dependencies().iter().any(|d| d == name))
            .map(|(n, _)| n.clone())
            .collect();
        if !dependents.is_empty() {
            return Err(KernelError::HasDependents {
                plugin: name.to_owned(),
                dependents: dependents.join(", "),
            });
        }

        let (_, Loaded { plugin, effect }) = self.loaded.remove(position);
        plugin.dispose(&self.ctx);
        drop(effect);
        Ok(())
    }

    /// 全部卸载，返回卸载顺序（加载顺序的倒序）。
    pub fn unload_all(&mut self) -> Result<Vec<String>, KernelError> {
        let mut names = Vec::with_capacity(self.loaded.len());
        while let Some((name, Loaded { plugin, effect })) = self.loaded.pop() {
            plugin.dispose(&self.ctx);
            drop(effect);
            names.push(name);
        }
        Ok(names)
    }

    /// 卸载再加载，返回插件名。
    pub fn reload(&mut self, name: &str) -> Result<String, KernelError> {
        let plugin = self
            .loaded
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, loaded)| Arc::clone(&loaded.plugin))
            .ok_or_else(|| KernelError::PluginNotLoaded(name.to_owned()))?;

        self.unload(name)?;
        self.load_shared(plugin)
    }

    /// 拓扑排序，返回的下标指向 `plugins`。
    ///
    /// 依赖要么在本次集合里，要么已经加载；成环时报出完整环路。
    fn resolve_order(&self, plugins: &[Arc<dyn Plugin>]) -> Result<Vec<usize>, KernelError> {
        let mut index: HashMap<&str, usize> = HashMap::with_capacity(plugins.len());
        for (i, plugin) in plugins.iter().enumerate() {
            let name = plugin.name();
            if index.insert(name, i).is_some() {
                return Err(KernelError::DuplicatePlugin(name.to_owned()));
            }
        }

        let mut deps: Vec<Vec<usize>> = Vec::with_capacity(plugins.len());
        for plugin in plugins {
            let mut resolved = Vec::new();
            for dependency in plugin.dependencies() {
                match index.get(dependency.as_str()) {
                    Some(&j) => resolved.push(j),
                    None if self.is_loaded(&dependency) => {}
                    None => {
                        return Err(KernelError::MissingDependency {
                            plugin: plugin.name().to_owned(),
                            dependency,
                        });
                    }
                }
            }
            deps.push(resolved);
        }

        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            New,
            Visiting,
            Done,
        }

        struct Sorter<'a> {
            deps: &'a [Vec<usize>],
            names: &'a [String],
            mark: Vec<Mark>,
            order: Vec<usize>,
            stack: Vec<usize>,
        }

        impl Sorter<'_> {
            fn visit(&mut self, node: usize) -> Result<(), KernelError> {
                match self.mark[node] {
                    Mark::Done => return Ok(()),
                    Mark::Visiting => {
                        // 从栈里回溯出环路，末尾补上又绕回来的那个节点。
                        let names = self.names;
                        let start = self
                            .stack
                            .iter()
                            .position(|&n| n == node)
                            .unwrap_or_default();
                        let mut cycle: Vec<&str> = self.stack[start..]
                            .iter()
                            .map(|&n| names[n].as_str())
                            .collect();
                        cycle.push(names[node].as_str());
                        return Err(KernelError::DependencyCycle {
                            cycle: cycle.join(" -> "),
                        });
                    }
                    Mark::New => {}
                }

                self.mark[node] = Mark::Visiting;
                self.stack.push(node);
                for i in 0..self.deps[node].len() {
                    let next = self.deps[node][i];
                    self.visit(next)?;
                }
                self.stack.pop();
                self.mark[node] = Mark::Done;
                self.order.push(node);
                Ok(())
            }
        }

        let names: Vec<String> = plugins.iter().map(|p| p.name().to_owned()).collect();
        let mut sorter = Sorter {
            deps: &deps,
            names: &names,
            mark: vec![Mark::New; plugins.len()],
            order: Vec::with_capacity(plugins.len()),
            stack: Vec::new(),
        };
        for node in 0..plugins.len() {
            sorter.visit(node)?;
        }
        Ok(sorter.order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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

    /// 有名字、依赖与 dispose 记录的插件，用来验证顺序。
    struct Named {
        name: &'static str,
        deps: Vec<String>,
        log: Arc<Mutex<Vec<String>>>,
    }
    impl Named {
        fn new(
            name: &'static str,
            deps: &[&str],
            log: &Arc<Mutex<Vec<String>>>,
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

    /// 注册了服务、且 dispose 会记账的插件：验证「失败回滚真的撤了注册」。
    struct Provider {
        name: &'static str,
        deps: Vec<String>,
        log: Arc<Mutex<Vec<String>>>,
    }
    impl Plugin for Provider {
        fn name(&self) -> &str {
            self.name
        }
        fn dependencies(&self) -> Vec<String> {
            self.deps.clone()
        }
        fn apply(&self, ctx: &Context) -> Result<Effect, KernelError> {
            ctx.provide(Greeting("hi"))
        }
        fn dispose(&self, _ctx: &Context) {
            self.log.lock().unwrap().push(self.name.to_owned());
        }
    }

    /// `apply` 直接报错的插件：用来把 `load_all` 逼进「加载到一半失败」那条路。
    struct Failing {
        name: &'static str,
        deps: Vec<String>,
    }
    impl Plugin for Failing {
        fn name(&self) -> &str {
            self.name
        }
        fn dependencies(&self) -> Vec<String> {
            self.deps.clone()
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

    #[test]
    fn dependencies_decide_load_and_unload_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());

        // 故意把依赖者排在前面，顺序必须由依赖而不是入参决定。
        let batch = vec![Named::new("B", &["A"], &log), Named::new("A", &[], &log)];
        assert_eq!(manager.load_all(batch).unwrap(), ["A", "B"]);
        assert_eq!(manager.loaded(), ["A", "B"]);

        // 卸载顺序反过来，dispose 也按那个顺序被调用。
        assert_eq!(manager.unload_all().unwrap(), ["B", "A"]);
        assert_eq!(*log.lock().unwrap(), ["B", "A"]);
    }

    #[test]
    fn cycles_are_rejected_without_leaving_half_loaded_state() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        let batch = vec![Named::new("A", &["B"], &log), Named::new("B", &["A"], &log)];

        let error = manager.load_all(batch).unwrap_err();
        let KernelError::DependencyCycle { cycle } = &error else {
            panic!("期望循环依赖错误，实际 {error}");
        };
        assert_eq!(cycle, "A -> B -> A");
        assert!(manager.is_empty(), "报错后不该留下半加载状态");
    }

    #[test]
    fn missing_dependency_is_an_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        let error = manager
            .load_shared(Named::new("B", &["A"], &log))
            .unwrap_err();
        assert!(matches!(error, KernelError::MissingDependency { .. }));
    }

    #[test]
    fn dependents_block_unload_but_reload_works() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        manager.load_shared(Named::new("A", &[], &log)).unwrap();
        manager.load_shared(Named::new("B", &["A"], &log)).unwrap();

        let error = manager.unload("A").unwrap_err();
        assert!(
            matches!(error, KernelError::HasDependents { .. }),
            "{error}"
        );

        // reload 先 dispose 再 apply，dispose 记录会多一条。
        assert_eq!(manager.reload("B").unwrap(), "B");
        assert_eq!(manager.loaded(), ["A", "B"]);
        assert_eq!(*log.lock().unwrap(), ["B"]);

        assert!(matches!(
            manager.load_shared(Named::new("A", &[], &log)),
            Err(KernelError::PluginAlreadyLoaded(_))
        ));
    }

    #[test]
    fn duplicate_names_in_one_batch_are_an_error() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut manager = PluginManager::new(Context::new());
        let batch = vec![Named::new("A", &[], &log), Named::new("A", &[], &log)];
        assert!(matches!(
            manager.load_all(batch),
            Err(KernelError::DuplicatePlugin(_))
        ));
    }

    /// 成环 / 重名 / 缺依赖都是在**加载开始前**就失败的，走不到回滚那条路。
    /// 这条专测「加载到一半失败」：A 的注册必须被撤掉，C 根本不该开始。
    #[test]
    fn a_failure_midway_through_load_all_rolls_back_earlier_plugins() {
        let ctx = Context::new();
        let mut manager = PluginManager::new(ctx.clone());
        let log = Arc::new(Mutex::new(Vec::new()));

        let batch: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(Provider {
                name: "A",
                deps: Vec::new(),
                log: Arc::clone(&log),
            }),
            Arc::new(Failing {
                name: "B",
                deps: vec!["A".to_owned()],
            }),
            Named::new("C", &["B"], &log),
        ];

        let error = manager.load_all(batch).unwrap_err();
        assert!(matches!(error, KernelError::ServiceMissing(_)), "{error}");

        assert!(
            manager.is_empty(),
            "失败后不该留下半加载状态：{:?}",
            manager.loaded()
        );
        assert!(
            matches!(ctx.get::<Greeting>(), Err(KernelError::ServiceMissing(_))),
            "A 注册的服务必须随回滚消失"
        );
        assert_eq!(
            *log.lock().unwrap(),
            ["A"],
            "A 该被加载过又 dispose；C 不该开始"
        );
    }
}
