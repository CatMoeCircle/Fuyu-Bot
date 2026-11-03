//! 运行期只读视图的存放点：三个查询接口（插件列表 / 插件详情 / TDLib JSON 请求）
//! 要从 **Java 调用线程**访问 worker 线程里的运行时，所以这里存一份视图。
//!
//! 视图由 `fuyubot-app` 提供（[`fuyubot_app::LiveView`]），只含 `Arc` 与不可变快照；
//! 本模块只管它的安装 / 摘除，以及「没在跑」时回 `None`。

use std::sync::Mutex;

use fuyubot_app::LiveView;

static LIVE: Mutex<Option<LiveView>> = Mutex::new(None);

/// 拿一份当前视图。克隆只复制几个 `Arc`，随用随取。
pub fn get() -> Option<LiveView> {
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 安装 / 摘除的守卫：**drop 就摘**。
///
/// 收摊、启动报错、panic（`catch_unwind` 之外）都算——视图绝不比运行时活得久，
/// 否则接口会拿着已经关掉的 TDLib 客户端去发请求。
pub struct LiveGuard;

impl LiveGuard {
    pub fn install(view: LiveView) -> Self {
        *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(view);
        Self
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        *LIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
