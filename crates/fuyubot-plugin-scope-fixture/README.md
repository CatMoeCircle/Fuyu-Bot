# fuyubot-plugin-scope-fixture

测试夹具：在 `plugin_start` 里注册一个私有服务、立刻取回来，再确认根上的公共服务
沿父链仍然可见。只服务 `fuyubot-loader` 的服务作用域集成测试。
