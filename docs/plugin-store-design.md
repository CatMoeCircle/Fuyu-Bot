# 插件商店与分发设计

> 读者：本仓维护者 + 商城后端实现者。
> 本文定义**契约**（目录格式、静态元数据、加载语义、发布与打包规范、索引格式），
> 并划清三件事的边界：**加载器不碰网络**、**商店客户端只在 TUI 里**、**商城后端不在本仓**。
> 代码落点见[附录 A](#附录-a本仓落点)。
>
> 状态标注：
>
> | 标记 | 含义 |
> | --- | --- |
> | ✅ | **已实现**：本仓当前代码就有（P0 全部落地，2026-11-03） |
> | 🟡 | **P0 契约已冻结、实施中**：历史标记，当前已无此项 |
> | ⬜ P0 | **P0 范围内、本仓、还没落地** |
> | ⬜ P1 | **不在本仓**：外部商城 / 索引托管，本文只给契约与建议 |
> | 💭 | **待拍板**：见 [§13](#13-需要拍板的点) |
>
> **阶段划分**（与本文其余部分一致）：
>
> - **分发 P0** = bundle 格式 + 加载器 + CLI 迁移 + **TUI 商店**（都在本仓）
> - **分发 P1** = 外部商城与索引托管（独立仓库 / 站点，**不在本仓**）
>
> ⚠️ **名字撞车**：`docs/ROADMAP.md` 里的 P0 / P1 / P2 指的是**内核 Fiber、`Context` 作用域链、观测性**那三件，
> 与本文的「分发 P0 / P1」不是一回事。本文一律写「分发 P0」「分发 P1」，避免再混。

---

## 0. 一句话

宿主只吃一种东西：**一个已经解压好的插件目录**。

这个目录可以从任何地方来——TUI 商店下载解压、作者 release 手动解压、`plugin install` 复制、
`plugin migrate` 迁移。**加载器只认目录，永远不碰网络与 zip。**

- **商店后端**（索引托管、收录审核、站点）：独立仓库 / 站点，不在本仓（分发 P1）。
- **商店客户端**（拉索引、搜索、下载、校验、解压、安装）：在本仓，但只活在
  `crates/fuyubot-cli/src/panel/store.rs` 这一个模块里（分发 P0 增量）。

分界就一句：**网络与 zip 只出现在 `panel/store.rs`，`fuyubot-loader` 不知道有网络这回事。**

---

## 1. 范围与分层

### 1.1 本仓做什么、不做什么

| 事 | 谁做 | 在本仓？ |
| --- | --- | --- |
| 定义 bundle 格式（`plugins/<id>/` + `plugin.json` + `bin/`） | 本仓 `fuyubot-loader` | ✅ / 🟡 |
| 静态探测（不 dlopen 就能列举、校验） | 本仓 `fuyubot-loader` | 🟡 |
| 加载 / 激活 / 卸载语义（含 Go 的特殊处理） | 本仓 | ✅ / 🟡 |
| `plugin list` / `install <目录>` / `uninstall` / `migrate` | 本仓 CLI | 🟡（现状是 dll 形态） |
| **TUI 商店客户端**：拉索引、搜索、详情、下载、sha256、解压、安装 / 卸载 | 本仓 CLI（`crates/fuyubot-cli/src/panel/store.rs`） | ⬜ P0 |
| 插件索引的**托管**、收录审核、站点 | **独立商城仓** | ⬜ P1 |
| zip 打包 | **插件作者** | 契约（本仓只给规范） |
| Release 托管与资产命名 | **作者 / 商城**（GitHub Releases） | 契约（本仓只给规范） |
| 其它宿主（APK 等）自己的商店 | **各宿主自己** | ❌（消费同一份 bundle 契约即可） |

一句话的分界：**本仓保证「解压好的目录能被正确加载」，也提供一条「把 zip 变成这个目录」的官方路径
（TUI 商店）；但这条路只在 `panel/store.rs` 里，加载器不知道它的存在。**

### 1.2 四层：哪一层动，哪一层不许动

把「插件」拆成四层，商店只影响后三层，ABI 一层不动：

| 层 | 内容 | 谁拥有 | 变动成本 |
| --- | --- | --- | --- |
| **ABI 层** | `fuyubot-abi` v5：四个导出符号（`plugin_init` / `plugin_start` / `plugin_stop` / `plugin_manifest`）+ `HostApiVTable` | 本仓 | **最高**：一动全部插件重编（见 [§13.1](#131-abi-兼容策略)） |
| **分发单元** | 一个目录 = 一个插件：`plugin.json` + `bin/`（+ `src/` `assets/` `config.default.toml`） | 本仓定义格式，任何安装器消费 | 中：改格式要迁移 |
| **静态元数据** | `plugin.json`（作者声明）、`index.json`（商城推荐）、`plugins.lock`（安装器账本） | 三方各写一份 | 低：加字段向后兼容 |
| **运行时** | `dlopen` → `plugin_init` → 内核 activation → `plugin_start` | 本仓 | 低：入口从 dll 变成 `entry` |

关键结论：**商店的复杂度不应该渗进 ABI。** 一个插件能装、能更新、能被索引，
靠的是「目录里有静态元数据」，不是「ABI 多几个函数」。

### 1.3 状态总表

| 项 | 层 | 状态 |
| --- | --- | --- |
| ABI v5（四符号 + VTable，精确版本校验） | ABI | ✅ 已实现，本文不动它 |
| 平铺 `plugins/*.dll` 发现 + `dlopen` 探测 | 运行时（兼容期） | ✅ 已实现，保留 |
| Go 插件**最后**加载（`unloadable` 分区） | 运行时 | ✅ 已实现（`crates/fuyubot-app/src/app.rs` 的加载队列） |
| Go 无 `plugin.json` → 拒绝，不 `dlopen` | 运行时 | ✅ `PluginLoader::probe` 读魔数即拒；`probe_for_migration` 是唯一例外 |
| `plugins/<id>/` + `plugin.json` + `bin/` 布局 | 分发单元 | ✅ 落地（脚本仍在 `plugins/scripts/<id>/`） |
| `BundleManifest` / `plugin_entries` / `probe_bundle`（`crates/fuyubot-loader/src/bundle.rs`） | 运行时 | ✅ 落地（`cargo test -p fuyubot-loader` 89 + 17 绿） |
| `plugin list` 不 `dlopen` | CLI | ✅ 落地（四段静态输出） |
| `plugin install <目录>` 装到 `plugins/<id>/` | CLI | ✅ 落地（按 kind 路由） |
| `plugin migrate [--dry-run]` | CLI | ✅ 落地（`fuyubot plugin migrate`） |
| 脚本插件仍在 `plugins/scripts/<id>/` | 分发单元 | ✅ 已实现，分发 P0 不动 |
| TUI 商店（拉索引 / 搜索 / 下载 / sha256 / 解压 / 安装 / 卸载） | CLI `panel` | ✅ 落地（`panel/store.rs`，21 条单测；[§12](#12-tui-商店分发-p0)） |
| 商城后端（索引托管、收录审核、站点） | — | ⬜ P1，**不在本仓** |
| 签名（minisign） | — | ⬜ P1（P0 只做 sha256（索引没有签名字段，不显示签名状态）） |
| 静态 `provides` 排序 | 运行时 | ⬜ P0，未排期（[§13.4](#134-静态-provides-排序是否值得做)） |
| `data/plugins/<id>/` 数据目录 | 运行时 | 💭 未定（[§9](#9-配置与数据分离)） |

> P0 已全部落地（2026-11-03）：契约见 [`plugin-format-v1.md`](plugin-format-v1.md)，
> 验证命令 `cargo test -p fuyubot-loader` / `cargo test -p fuyubot-cli` / `cargo build --workspace`。

### 1.4 谁写、谁读、信谁

商店生态里最容易出的错是**把某一方的话当成事实**。四份数据各有权威边界：

| 数据 | 谁写 | 它是什么 | 本仓怎么用 |
| --- | --- | --- | --- |
| `plugin.json` | 插件作者 | 作者的**声明**，可能过期、可能写错 | `probe` 读它；`abi` 只是**预检建议值**，真值以 `plugin_init` 返回的 handle 为准 |
| `index.json` | 商城 CI | 商城对外的**推荐**，不是本地事实 | **加载路径不读**；TUI 商店读它，但装完仍以本地目录为准（[§12](#12-tui-商店分发-p0)） |
| 已解压的 `plugins/<id>/` | TUI 商店 / 外部安装器 / 用户 | **本地事实** | 本仓唯一信任的东西 |
| `plugins.lock` | 外部安装器（可选） | 复现凭据 | **本仓不读** |

推论（写进契约，别在实现里让步）：

- **索引说 `abi: 5` 不代表真能加载**。宿主必须按真实 handle 判定，装前预检只是「明显不匹配就别装」。
- **商城挂了不影响宿主启动**。本仓的**加载路径**只读本地目录、零网络（[§5.4](#54-为什么客户端绝不打-github-api)）。
- **锁文件不是事实来源**，是「上次装了什么」的备忘；与磁盘不一致时以磁盘为准并提示重建。

---

## 2. 目录布局（分发 P0）

```text
plugins/
├── <id>/                       # 原生插件包（新形态）
│   ├── plugin.json             # 必需：静态元数据
│   ├── bin/                    # 动态库 + 它的兄弟库
│   │   └── fuyubot_plugin_x.dll
│   ├── src/                    # 可选：脚本 / 资源（安装器原样保留）
│   ├── assets/                 # 可选
│   ├── config.default.toml     # 可选：默认配置（只读，见 §9）
│   └── README.md / LICENSE     # 可选
├── scripts/                    # 脚本插件根（分发 P0 不动）
│   └── <id>/
│       ├── plugin.json         # 可选：宿主只读它做静态列举
│       ├── package.json        # 或 pyproject.toml（真正的发现依据在中间插件那边）
│       └── ...
├── .disabled/<id>/             # 被禁用的包（不在扫描路径里）
├── .staging/                   # 商店 / 安装器的临时目录（宿主跳过）
├── .trash/                     # 预留：未来原地更新时放旧目录（P0 商店不创建）
├── .cache/                     # 可丢弃缓存（宿主跳过）
├── weather.dll                 # 旧形态：平铺原生插件（兼容期）
└── weather.dll.disable         # 旧形态：被禁用的平铺插件
```

规则（全部是**本仓读**的规则；写这些目录的只有 `install` / `migrate` 这两个用户显式动作）：

- 原生包扫描 `plugins/*/plugin.json`，**只下一层，不递归**。
- 脚本包扫描 `plugins/scripts/*/plugin.json`，同样只下一层，**只用于列举**；
  真正的发现 / 启停仍归 SDK 的 `fuyubot_python_host` / `fuyubot_js_host`。
- **以 `.` 开头的目录一律跳过**（`.disabled` / `.staging` / `.trash` / `.cache` / `.git`）。
- 目录名**必须等于** `plugin.json` 的 `id`；不一致时**以 `id` 为准并告警**（不拒绝加载）。
- 禁用 = 整个目录挪进 `plugins/.disabled/<id>/`，**不写第二份清单**。
  旧形态的禁用仍是改名 `foo.dll` → `foo.dll.disable`，两种并存到兼容期结束。
- 扫描顺序：bundle 按 `id` 字典序在前，legacy 按文件名在后（`plugin_entries` 的契约）。

### 2.1 为什么脚本插件不在分发 P0 搬家

脚本插件的「家」目前是 `plugins/scripts/<id>/`，而**认领它的是两个中间插件**（SDK 仓），
扫描根写死在 SDK 侧。把脚本包挪到 `plugins/<id>/` 意味着跨仓改契约（`AGENTS.md` 约束 51 的发布顺序也要跟着动），
收益只是「目录树更整齐」。所以分发 P0 明确不动，把统一时机留给 [§13.2](#132-目录统一时机)。

---

## 3. `plugin.json`：静态元数据

### 3.1 示例

```json
{
  "schema": 1,
  "id": "com.example.weather",
  "name": "Weather",
  "version": "1.2.0",
  "kind": "native",
  "entry": "bin/fuyubot_plugin_weather.dll",
  "abi": 5,
  "platforms": ["windows-x86_64", "linux-x86_64"],
  "depends": ["fuyubot.help.commands"],
  "optional_depends": [],
  "provides": ["weather.api"],
  "config": "config.default.toml",
  "description": "按城市查天气并回复",
  "license": "MIT",
  "homepage": "https://github.com/example/fuyubot-weather"
}
```

### 3.2 字段语义与校验

| 字段 | 必填 | 语义 | 校验（P0，失败即 `BundleError`） |
| --- | --- | --- | --- |
| `schema` | 是 | 格式版本 | 只认 `1`；更大 → `UnsupportedSchema` |
| `id` | 是 | 全局唯一标识，也是目录名 | `[A-Za-z0-9][A-Za-z0-9._-]*`，≤128，不含路径分隔符，不以 `.` 开头 → 否则 `BadId` |
| `name` | 是 | 展示名（可中文） | 非空 |
| `version` | 是 | 作者版本号 | 非空，只允许 `[0-9A-Za-z.+-]`；P0 **不强制 semver**（商城阶段建议强制，[§13.8](#138-其它小项)） |
| `kind` | 是 | `native` / `python` / `js` | 枚举外 → 解析失败 |
| `entry` | 是 | 相对包根的真实入口 | 禁绝对路径、`..`、反斜杠开头；解析后必须仍在包内且是文件 → `BadEntry` / `EntryMissing` / `EntryEscapes` |
| `abi` | native 建议 | 静态预检用的**建议值** | 缺失 = 未知（`Probe.abi_version` 为 0，`plugin list` 显示 `?`） |
| `platforms` | 否 | 空 / `any` = 全平台 | 否则必须含 `current_platform()` → `UnsupportedPlatform` |
| `depends` | 否 | 硬依赖：**服务名**，进内核 `inject` | 空字符串忽略 |
| `optional_depends` | 否 | 软依赖：进 `soft_inject` | 同上 |
| `provides` | 否 | 本插件提供的服务名 | 目前**只用于列举 / 商城展示**（[§4.4](#44-inject-是服务名排序要靠-provides)） |
| `config` | 否 | 默认配置相对路径 | P0 **只解析、不合并**（[§9](#9-配置与数据分离)） |
| `description` / `license` / `homepage` | 否 | 展示用 | 不校验 |
| 其它 | — | 未知字段 | **忽略**（向前兼容，别在解析器里 `deny_unknown_fields`） |

`current_platform()` 返回 `windows-x86_64` / `linux-x86_64` / `macos-aarch64` 这类串，
**与发布资产命名里的 `<os>-<arch>` 完全一致**——这两处一旦漂移，
`platforms` 写对了也会被判成不匹配（[§5 常见错误](plugin-migration.md#5-常见错误排查)）。

### 3.3 商城阶段建议更严（不在本仓）

商城 CI 可以在收录时加严，宿主侧不加：

- `id` 用反向域名（`com.example.weather`），且与商城条目一一对应；
- `version` 强制 semver（索引要排序、要算「有没有新版」）；
- native 的 `entry` 必须在 `bin/` 下（打包规范，[§7](#7-zip-布局与解压安全发布契约)）；
- `abi` 必填，且必须等于当前宿主 ABI（商城按宿主版本给不同索引，见 [§13.1](#131-abi-兼容策略)）。

---

## 4. 加载语义

### 4.1 三个时刻

```text
scan（plugin_entries）     读目录 + plugin.json，不 dlopen
   │                        顺序：bundle 按 id 字典序，legacy 按文件名
   ▼
probe（probe_bundle / probe）  静态包：读 plugin.json + entry 魔数；legacy：仍 dlopen 一次
   │                        失败 → 只跳过这一个，warn 出原因，不阻断启动
   ▼
activate（内核 PluginManager → DylibPlugin::apply）
                             start_library(probe.entry) → plugin_init → plugin_start
```

三条必须守住的性质：

1. **`probe_bundle` 绝不 `dlopen`**。这不是优化，是语义：
   「不执行就能知道它是谁」是新格式相对平铺 dll 最实质的好处。
   一个装进来的插件不该为了被列举而跑一遍代码。
2. **真正 load 只发生在内核 activation**。`DylibPlugin::apply` 用 `self.entry`
   （不是 `self.path`）去 `start_library`——bundle 的 `path` 是目录，`entry` 才是库。
3. **单个插件失败不阻断其余**。扫描 / 探测 / 加载逐项独立，
   一个坏目录的后果是「这个插件没起来 + 一条带路径的 warn」，不是「bot 起不来」。

### 4.2 `probe` 不 dlopen 的边界

| 形态 | 探测方式 | 会执行插件代码吗 |
| --- | --- | --- |
| `plugins/<id>/`（bundle） | 读 `plugin.json` → 校验 → 读 `entry` 文件魔数判 Go | **不会** |
| `plugins/foo.dll`（legacy） | 仍然 `dlopen` + `plugin_init` 问名字（探测完立刻 `stop` / `free_handle`，注册整批回滚） | **会** |
| `plugins/foo.dll` 且是 Go | **不 dlopen**，直接 `GoRequiresManifest` | 不会 |

legacy 那一行是兼容期的**已知代价**：平铺 dll 的名字只有运行时才知道（文件名与插件名常常不是一回事），
宿主没有先验手段。这也是「迁移到 bundle」最实在的理由之一——迁移之后探测不再执行代码。

### 4.3 Go：为什么必须拒绝、为什么最后加载

Go 的 `-buildmode=c-shared` 有两条硬限制（`AGENTS.md` 约束 50）：

- **不能卸载**：`dlclose` 之后 Go runtime 的线程再跑一步就是访问违例，所以宿主识别出 Go 就**故意泄漏**句柄到进程结束；
- **同进程重复加载不可靠**：再 `dlopen` 一次拿到的不一定是「新」的那份。

于是「先 `dlopen` 看看它是谁」对 Go 是**不可逆**的：一次探测 = 永久映射 + 后续无法重载。
所以顺序只能是「先有静态元数据，才允许加载」：

```rust
// 契约里的错误（docs/plugin-format-v1.md §5）
#[error("Go 插件 {path} 没有 plugin.json：它无法卸载，必须先有静态元数据才能安全加载")]
GoRequiresManifest { path: PathBuf },
```

**最后加载**是另一件事：`fuyubot-app` 把 `probe.go_runtime` 为真的插件排到队列末尾，
让可卸载的先 settle。依赖顺序仍由内核 fiber 决定，这里只保证「不可逆的那一步晚发生」。

两条补充（`AGENTS.md` 约束 60）：

- **唯一允许 `dlopen` Go 的例外是 `probe_for_migration`**（`plugin migrate` 用）。理由：迁移是用户显式触发、
  进程马上退出，永久映射没有后果；但调用方**必须**提示「本次迁移进程会永久映射」。
- 同一个进程里**别放多个会装信号处理器的 Go 插件**——它们会互相抢 handler。

对商店的推论（写进商城文档）：**Go 插件的「更新」等于「重启进程」**，
安装器不能承诺「装完即生效」，也不能在同一进程里做「先卸再装」。

### 4.4 `inject` 是服务名，排序要靠 `provides`

- 内核的 `inject` / `soft_inject` 收的是**服务名**（`Context` 按名字寻址的服务表），
  不是插件名、不是文件路径、不是包 id。
- bundle 的 `depends` / `optional_depends` 在 `Probe.manifest` 里合成成
  `Manifest { inject, soft_inject }`——也就是说 **bundle 不引入新的依赖机制**，只是把声明从运行时搬到了静态文件。
- 优先级（`AGENTS.md` 约束 53）：`plugin_manifest` 该侧非空 → 用它；否则回退 `[plugin.<名>].inject`。
  bundle 的 `depends` 非空就**覆盖**配置里那份，逐侧判定。
- 激活顺序由内核 `reconcile` 按「服务是否可用」推进，**不是**按拓扑排序一次性算好；
  `reconcile` 不许在 `apply` 期间递归推进（约束 54）。

**`provides` 目前只用于列举与商城展示**，不参与排序。
「静态排序」（不 `dlopen` 就把加载顺序排出来）需要 `provides` 建图，属于本仓未排期的增量（⬜ P0），
做法与代价见 [§13.4](#134-静态-provides-排序是否值得做)。

### 4.5 两个名字，两道不同的校验

bundle 里同时存在两个「名字」，别混：

| 比较 | 不一致时 | 为什么 |
| --- | --- | --- |
| **目录名** vs `plugin.json` 的 `id` | **以 `id` 为准 + 告警**，照常加载 | 目录可以被人手工改名；`id` 才是身份。`uninstall` 按 `id` 找，所以建议改对 |
| **`plugin_init` 报的名字** vs `plugin.json` 的 `id` | **拒绝加载**（`AGENTS.md` 约束 59） | 静态元数据是依赖、`[plugin.<id>]` 配置、商店记账的共同键；运行时名字对不上，说明包装错了或清单写错了，继续加载会让三样东西全部错位 |

第二条是硬拒绝，不是告警：`plugin.json` 说自己是 A、`plugin_init` 说自己是 B，
无论信哪个都会有一半东西指错。这种包在**安装阶段**就该被 TUI 商店的 `validate` 拦下来
（[§12.3](#123-安装流水线)），加载阶段只是最后一道闸。

---

## 5. 商城架构：后端在 P1，客户端在 P0

> - **后端**（索引托管、收录审核、站点）：独立仓库 / 站点，**不在本仓**（分发 P1）。
> - **客户端**（拉索引、下载、sha256、解压、安装）：本仓 TUI（分发 P0，`panel/store.rs`，见 [§12](#12-tui-商店分发-p0)）。
>
> 本节描述**两边共同遵守的数据契约**：两层数据、缓存、限流。

### 5.1 为什么数据分两层

| 层 | 放什么 | 为什么不能合并 |
| --- | --- | --- |
| **索引层** | `index.json`：谁存在、有哪些版本、每个版本的资产 URL 与 sha256 | 一个文件就能「浏览全市场」，不必逐个仓库去问 |
| **资产层** | GitHub Releases 上的 `<id>-<version>-<os>-<arch>.zip` | 二进制体积大、要 CDN、要长期不变；Releases 的资产 URL 是**稳定精确 URL** |

合并成一层（比如「索引里直接塞 zip」）会立刻撞上仓库体积与 CDN 成本；
合并成「客户端去查 GitHub API」会撞上限流（[§5.4](#54-为什么客户端绝不打-github-api)）。

### 5.2 索引层

- 托管：GitHub Pages（`https://<org>.github.io/<repo>/index.json`）为主，`raw.githubusercontent.com` 为备。
- 缓存：**ETag + TTL**（建议 6 小时）+ 本地缓存；`304` 直接用缓存，不重复解析。**这是 P1 的目标**；
  P0 的 TUI 商店只在内存里拉一次（进商店页 / 按 `r`），不落盘。
- 索引是**纯元数据**：不含插件源码、不含 zip 本体、不含 `releases/latest` 这类会变的链接。
- 索引挂了 / 拉不到 / 解析失败 → TUI 商店保留旧索引、显示「r 重试」，**不报致命错误**。

### 5.3 资产层

- 每个版本每个平台一个 zip，URL 形如
  `https://github.com/<owner>/<repo>/releases/download/<tag>/<asset>`；
- 索引里存**精确 URL + sha256 + size**，不存「仓库名 + 让我去查最新 release」；
- `size` 用于下载前预检与进度显示；`sha256` 是**唯一**的完整性依据（见 [§7.4](#74-解压实现要求tui-商店与外部安装器通用)）。

### 5.4 为什么客户端绝不打 GitHub API

GitHub REST API 未认证是 **60 次/小时/IP**。一个「刷新索引」按钮在 NAT 后面被点几次就 403，
而用户看到的是「商店坏了」。所以：

- 索引走 Pages / raw（CDN，无此限流）；
- 资产走 `releases/download/<tag>/<asset>`（精确 URL，无 API）；
- **`api.github.com` 只允许出现在商城 CI 里**（发版、生成索引），不出现在任何客户端路径上——
  包括 TUI 商店。

### 5.5 缓存与失效

| 缓存 | P0 现状 | 可删？ |
| --- | --- | --- |
| 索引 | **只在内存**（进商店页拉一次、`r` 重拉）；不落盘、不做 ETag | — |
| 下载的 zip | **只在内存**（≤50MB，校验后直接解压）；不落临时文件 | — |
| 解压 staging | `plugins/.staging/<id>/`（宿主会跳过） | 可以 |

`plugins/.cache/` 是给后续版本（索引 / 资产缓存）预留的，P0 不创建。
不管谁写缓存，都**别写进 `plugins/<id>/`**——那是只读区（[§9.3](#93-插件目录安装后只读)）。

---

## 6. Release 规范（发布契约）

> 作者按这一节打包，商城 CI 按这一节校验。本仓不解析这些，只在文档里定义。

### 6.1 tag

| 仓库形态 | tag | 例 |
| --- | --- | --- |
| 一个仓库一个插件 | `v<version>` | `v1.2.0` |
| 一个仓库多个插件（monorepo） | `<id>-v<version>` | `com.example.weather-v1.2.0` |

规则：

- tag 里的 `<version>` 必须与 zip 内 `plugin.json` 的 `version` **逐字相同**（含 `+build` 元数据）；
- tag 一旦发布**不许移动**（同一 tag 换资产 = 用户装出来的字节与索引不符）；
- 预发布用 GitHub 的 prerelease 标记，索引默认不收录 prerelease。

### 6.2 资产命名

```text
<id>-<version>-<os>-<arch>.zip      # 平台相关：windows-x86_64 / linux-x86_64 / macos-aarch64 …
<id>-<version>-any.zip              # 平台无关：脚本插件、纯数据插件
```

例：

```text
com.example.weather-1.2.0-windows-x86_64.zip
com.example.weather-1.2.0-linux-x86_64.zip
com.example.weather-1.2.0-any.zip
```

- `<os>-<arch>` 与 `plugin.json` 的 `platforms`、宿主的 `current_platform()` **同一套字面量**；
- 一个 release 可以带多个平台资产，**不要**打「全平台一个 zip 里塞三个 dll」；
- zip 文件名里的 `<id>` 必须等于 `plugin.json` 的 `id`（商城 CI 校验，防「文件名钓鱼」）。

### 6.3 `SHA256SUMS`

每个 release 附一份 `SHA256SUMS`，**GNU `sha256sum` 格式**（`<hex>  <文件名>`，两个空格）：

```text
9f2c…（64 位十六进制）  com.example.weather-1.2.0-windows-x86_64.zip
1a7b…（64 位十六进制）  com.example.weather-1.2.0-linux-x86_64.zip
```

理由：本仓 `crates/fuyubot-tdlib-sys/src/build_support.rs` 的 `digest_for` 已经在解析这种格式
（TDLib 资产的 `SHA256SUMS`），商城 CI 可以复用同一套口径，不必发明第二种校验表。

> ⚠️ **校验不许静默退化**：索引 / `SHA256SUMS` 缺失或格式不对时，
> 商店 / 安装器要**拒绝安装并说明原因**，不能像旧的 `<资产>.sha256` 侧车那样「找不到就跳过校验」
> （`docs/pitfalls.md` 坑 97 记的就是这个）。

### 6.4 可选 minisign

- `SHA256SUMS.minisig`：对 `SHA256SUMS` 的 detached 签名；
- 公钥由商城 / 宿主**预置**（配置可覆盖），**不从被签名的同一来源取**；
- 没有签名时，商店 / 安装器至少要显示**来源 URL**；P0 索引没有签名字段，不要显示成「已校验」。 

签名**怎么做、要不要在 TUI 商店里强制**见 [§13.3](#133-tui-商店的依赖与签名)；
本节只定「资产旁边该有什么」。

---

## 7. zip 布局与解压安全（发布契约）

### 7.1 布局：根 = 插件目录内容

```text
com.example.weather-1.2.0-windows-x86_64.zip
├── plugin.json          ← 必须在根，不许套一层目录
├── bin/
│   └── fuyubot_plugin_weather.dll
├── config.default.toml  ← 可选
├── assets/              ← 可选
├── README.md            ← 可选
└── LICENSE              ← 可选
```

**不套一层**：解压出来就是 `plugins/<id>/` 的内容。这条与发行包
（`fuyubot-<版本>-<OS>-<平台>.zip`，解压即本体，`AGENTS.md` 约束 49）是同一个规矩——
生态里「有的包套一层、有的不套」是最烦人的那类不一致。

脚本插件的 zip 额外允许 `src/`、`package.json` / `pyproject.toml` 与 lock 文件，
但**仍然不许**带 `node_modules` / `.venv`（安装期由中间插件现装）。

### 7.2 允许 / 禁止清单

| 允许 | 禁止 |
| --- | --- |
| `plugin.json`（根，必需） | `node_modules/`、`.venv/`、`target/`、`__pycache__/`、`.git/`、`dist/` |
| `bin/`（动态库 + 兄弟库） | 符号链接（`unix mode 0o120000`） |
| `src/`、`assets/` | 绝对路径、`..` 路径段、盘符（`C:`）、NTFS ADS（`:`） |
| `config.default.toml` | Windows 保留名（`CON` / `PRN` / `AUX` / `NUL` / `COM1..9` / `LPT1..9`） |
| `README*`、`LICENSE*`、`CHANGELOG*` | 尾随空格 / 尾随点的文件名 |
| 脚本插件的 `package.json` / `pyproject.toml` / lock | 大小写不同的同名条目（`Bin/X.dll` vs `bin/x.dll`） |

最后一条不是洁癖：Windows 文件系统大小写不敏感，两个「不同」条目解压后会互相覆盖，
装出来的东西与作者本机不一致，且**没有任何报错**。

### 7.3 限额（与契约一致）

| 项 | 上限 | 说明 |
| --- | --- | --- |
| 单包（zip 本身） | 50 MB | 超了基本是打错了包 |
| 总解压后 | 200 MB | 同上 |
| 条目数 | 2000 | 防「十万个小文件」拖死安装 |
| 压缩比 | > 200:1 告警 | 纯文本天然高压缩比，**只告警不拒绝**，真正兜底靠总量上限 |

⚠️ 数字以 [`docs/plugin-format-v1.md`](plugin-format-v1.md) §7 为准，改一处要两处同步。

⚠️ **限额必须在流式解压时边解边算**，不能先信 zip 头里的 `uncompressed_size`——
那个字段是攻击者可控的，先信它再分配缓冲区就是最经典的 zip bomb 打法。

### 7.4 解压实现要求（TUI 商店与外部安装器通用）

1. 先按 sha256 校验整包，**通过后才解压**；
2. 解压到 `plugins/.staging/<id>-<version>-<rand>/`（与目标同卷，后面才能原子 rename）；
3. 逐条目检查：路径归一化（`\` → `/`）、拒绝 `..` / 绝对路径 / 符号链接 / 保留名、大小写折叠查重；
4. 解压完用 `BundleManifest` 再校验一次（`id` / `version` / `entry` 存在 / `platforms` / `abi`）；
5. 通过后才 `rename` 到 `plugins/<id>/`（[§8.3](#83-安装流水线tui-商店与外部安装器共用)）。

任何一步失败都要**清掉 staging**，不能留下半个 `plugins/<id>/`——
宿主虽然能容忍（[§8.2](#82-宿主对半成品的容错)），但用户会看到一个「有时在有时不在」的插件。

---

## 8. 安装 / 更新 / 卸载

### 8.1 本仓在分发 P0 有四个入口

| 入口 | 做什么 | 不做什么 |
| --- | --- | --- |
| `fuyubot plugin install <目录>` | 把**已解压的**目录复制进 `plugins/<id>/`（`kind = python/js` 仍进 `plugins/scripts/<id>/`） | 不下载、不解压、**不认 zip** |
| `fuyubot plugin uninstall <名字>` | 按 `plugins/<id>/` → `plugins/scripts/<id>/` → `.disabled/` 两处 → 平铺 dll（带不带 `lib` / `.disable`）顺序找到并删掉 | 不删 `fuyubot.toml` 里的 `[plugin.<id>]`，不删数据 |
| `fuyubot plugin migrate [--dry-run]` | 把平铺 dll 搬进 `plugins/<id>/bin/` 并生成 `plugin.json` | 不联网、不合并已有目录 |
| **TUI 商店**（`panel/store.rs`） | 拉索引 → 搜索 / 详情 → 下载 → sha256 → 解压 → 安装 / 卸载 | 不改加载语义、不把 zip 概念漏给 loader |

四个入口写出来的都是**同一种东西**：`plugins/<id>/`。这就是整个设计的收敛点。

`plugin install` 收到 **zip** 时的行为：**明确报错**，指路「TUI 商店（`fuyubot run --tui` 的商店页），
或先解压再 `plugin install <目录>`」。
⚠️ 契约原文写的是「zip 安装属于 P1」；按现在划定的范围，zip 的**唯一入口**是 TUI 商店
（或用户手动解压），CLI 子命令永远不吃 zip（[§13.5](#135-zip-的入口只有一个)）。

### 8.2 宿主对「半成品」的容错

安装可能由别的进程（TUI 商店 / 外部安装器）在做，宿主必须假设目录可能不完整：

- `plugin.json` 缺失 / 解析失败 → 该目录不是 bundle，**跳过并 warn**（不猜它是什么、不拿目录名当 id）；
- `entry` 不存在 → `EntryMissing`，跳过这一个；
- 目录名 ≠ `id` → 告警后按 `id` 加载；
- 目录里有 `.staging` / `.cache` 残留 → 以 `.` 开头，天然跳过；
- **一个坏目录不影响其它插件，也不影响启动**（`probe` 失败只 `continue`）。

这条容错是「商店 / 安装器可以随便折腾」的前提，不要为了「严格」把它收紧成「发现坏目录就拒绝启动」。

### 8.3 安装流水线（TUI 商店与外部安装器共用）

```text
resolve   索引里挑 <id>@<version> + 目标平台资产
  ▼
fetch     精确 URL 下载 zip（ETag 只管索引；资产 URL 是不可变的）
  ▼
verify    sha256（+ 可选 minisign）——不过就停
  ▼
stage     解压到 plugins/.staging/<id>/，边解边查 §7.3 限额
  ▼
validate   BundleManifest 校验：id / version / entry 存在 / platforms 含本机 / abi 与宿主匹配
  ▼
swap      同卷 rename：staging → plugins/<id>（或 plugins/scripts/<id>）
  ▼
cleanup   清 .staging/<id>（成功时它已经被 rename 走）
  ▼
(可选) 写 plugins.lock
```

要点：

- **原子落盘靠同卷 `rename`**：所以 staging 必须放在 `plugins/` 下面（跨卷 rename 会退化成复制，不再原子）；
- **`id` 决定目标目录名**，不是 zip 文件名——zip 名可以被改，`plugin.json` 是内容；
- **不合并**：目标已存在就**报错**「先卸载旧版本（按 x）」，绝不「解压覆盖到旧目录上」；
  P0 的「更新」= 先卸载再安装（不自动更新，见 §12.8）；`.trash` 是后续版本的预留，P0 商店不创建。

### 8.4 两条硬边界：Windows 映射、Go 不可卸载

| 边界 | 现象 | 正确做法 |
| --- | --- | --- |
| **Windows 上已映射的 dll 删不掉** | 删 / 替换正在被加载的 dll 时报「拒绝访问」/ sharing violation | 装新插件、卸脚本插件**可以在运行中做**（重启生效）；只有**替换 / 删除已映射的原生 dll** 必须停机——按报错提示 `fuyubot stop`，别预先一刀切 |
| **Go 插件永不卸载** | 更新后进程里还是旧的那份 | 更新 Go 插件 = 重启进程；商店不能承诺「装完即生效」 |

推论（写进商城 FAQ）：**分发 P0 没有热更新**。原生插件装完必须重启；
脚本插件由中间插件的 watch 热加载，那是 SDK 侧的行为，与本契约无关。

### 8.5 可选账本 `plugins.lock`（不在本仓）

如果安装器想「换台机器装出一模一样的字节」，可以维护一份账本。格式建议 TOML，放项目根（与 `fuyubot.toml` 同级）：

```toml
# 由安装器生成，本仓不读
version = 1

[[plugin]]
id = "com.example.weather"
version = "1.2.0"
platform = "windows-x86_64"
sha256 = "…64 位十六进制…"
source = "https://github.com/example/fuyubot-weather/releases/download/v1.2.0/com.example.weather-1.2.0-windows-x86_64.zip"
entry = "bin/fuyubot_plugin_weather.dll"
abi = 5
installed_at = "2026-11-03T12:00:00Z"
```

- 它是**派生数据**，可随时重建；与磁盘不一致时以磁盘为准；
- 它的价值是「按 sha256 取，不按 latest 取」——所以重装是**可复现**的；
- 本仓不读它，也不校验它（[§1.4](#14-谁写谁读信谁)）。

---

## 9. 配置与数据分离

### 9.1 数据分四类，别混在一起

| 类别 | 位置 | 谁写 | 随 zip 分发 | 卸载时 |
| --- | --- | --- | --- | --- |
| **默认配置** | `<包>/config.default.toml` | 作者 | ✅ | 随目录删 |
| **用户配置** | `fuyubot.toml` 的 `[plugin.<id>]` | 用户 / 插件的 `set_config` | ❌ | **保留** |
| **运行时数据** | `data/plugins/<id>/`（💭 未定，见下） | 插件自己 | ❌ | **保留**（`--purge` 才删） |
| 可丢弃缓存 | `plugins/.cache/`（预留） | 宿主 / 安装器 | ❌ | 可随时删 |

原则：**bundle 只带默认值，不带用户状态。** 装一个插件不该覆盖用户的配置，
卸一个插件也不该删用户的数据。

### 9.2 默认配置怎么用（P0 只解析）

分发 P0 的契约是「`config` 字段只解析、不合并」。往后有两种做法，选一个拍板：

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| A 写进用户配置 | 首次安装把默认值补进 `[plugin.<id>]`（只补缺的键） | `set_config` 落盘是 `toml::to_string_pretty` **全量重写**（约束 52），注释会丢；写进去的键用户再也分不清「默认」还是「我设的」 |
| B 运行时兜底 | 默认值留在包里，`get_config` 未命中时宿主查该 owner 的 bundle 默认值 | 宿主要在 `get_config` 里多一次「section → bundle」映射；语义更干净，用户配置里只有用户真正设过的键 |

建议 **B**。A 会在用户配置里留下一堆「删不掉的默认值」，而且与「注释会丢」的全量重写叠在一起，
每次保存都在改写用户的文件。

### 9.3 插件目录安装后只读

- 宿主与插件**都不往 `plugins/<id>/` 里写**（日志、缓存、数据库一律不落这里）；
- 想写东西：配置走 `set_config`，数据走 `data/plugins/<id>/`（💭）或用户配置里指定的路径；
- 「只读」目前是**约定**，不是强制（没有 chmod / ACL）。真要强制是另一个量级的改动，
  而且 Windows 上给目录设 ACL 会把「用户想手动改个文件」也一起挡住——先按约定走。

⚠️ 现状提醒：今天插件的相对路径落在**项目根**（`run` 在加载插件前切了 `cwd`，
`crates/fuyubot-cli/src/main.rs`），所以「插件把缓存写到配置文件旁边」是当前的实际行为。
`data/plugins/<id>/` 还没实现，别在文档里写成既成事实。

---

## 10. 索引条目示例与商城治理（分发 P1 · 后端不在本仓）

> 索引的**字段语义以 [`docs/plugin-format-v1.md`](plugin-format-v1.md) §7 为准**（TUI 商店按它消费）；
> 本节补的是「示例 + 商城怎么产出它」。

### 10.1 `index.json` 示例

```json
{
  "schema": 1,
  "plugins": [
    {
      "id": "com.example.weather",
      "name": "Weather",
      "summary": "按城市查天气并回复",
      "description": "支持 /weather 命令与定时推送",
      "repo": "https://github.com/example/fuyubot-weather",
      "license": "MIT",
      "homepage": "https://github.com/example/fuyubot-weather#readme",
      "tags": ["weather", "http"],
      "latest": "1.2.0",
      "versions": [
        {
          "version": "1.2.0",
          "api": ">=1.0, <2.0",
          "abi": 5,
          "depends": ["fuyubot.help.commands"],
          "yanked": false,
          "assets": {
            "windows-x86_64": {
              "url": "https://github.com/example/fuyubot-weather/releases/download/v1.2.0/com.example.weather-1.2.0-windows-x86_64.zip",
              "sha256": "<64 位十六进制>",
              "size": 1843200
            },
            "linux-x86_64": {
              "url": "https://github.com/example/fuyubot-weather/releases/download/v1.2.0/com.example.weather-1.2.0-linux-x86_64.zip",
              "sha256": "<64 位十六进制>",
              "size": 1798144
            },
            "any": {
              "url": "https://github.com/example/fuyubot-weather/releases/download/v1.2.0/com.example.weather-1.2.0-any.zip",
              "sha256": "<64 位十六进制>",
              "size": 1204224
            }
          }
        }
      ]
    }
  ]
}
```

字段约定（契约 §7 的要点）：

- `assets` 是**按平台名做键的对象**，不是数组；平台名与 `current_platform()` 同一套字面量；
- 选择顺序：**精确匹配当前平台 → 回退 `any` → 都没有 = 该平台不可用**；
- `sha256` **必填**（大小写不敏感）；`size` 可选，只做显示与预检；
- `abi` 大于宿主 `ABI_VERSION` → 明确报「需要更新的宿主」，**不下载**；
- `yanked: true` 的版本不参与选择；
- `api` 是版本兼容区间，与 `abi` 不是一回事（`abi` = VTable 版本，加载时精确相等）；
- 下载 URL 只允许 `https`；
- 未知字段忽略 → 商城可以自行加 `generated_at` / `provides` 之类的扩展字段，客户端别依赖它们。

商城侧的约定：

- `url` 由**商城 CI 生成**，不接受提交者手写（防钓鱼、防写错）；
- `sha256` / `size` 由 CI 下载资产后计算，与作者提交的 `SHA256SUMS` 交叉校验；
- `latest` 只是展示用，**装哪个版本必须显式指定**（别把 `latest` 当安装依据）；
- 一个插件一个条目，多版本在 `versions` 里；不同 ABI 的同一版本算**不同条目**（[§13.1](#131-abi-兼容策略)）。

### 10.2 商城仓布局（建议）

```text
FuyuBot-plugins/
├── plugins/
│   └── com.example.weather.toml    # 一条提交 = 一个文件（少冲突）
├── schema/
│   └── submission.schema.json
├── scripts/
│   └── build-index.mjs             # 生成 index.json
├── index.json                      # 产物，发布到 Pages
└── .github/workflows/
    ├── validate-pr.yml             # PR：下载 + 校验 + 干跑
    └── publish.yml                 # main：生成 + 签名 + 发布
```

### 10.3 治理流程

```text
作者提 PR（提交 plugins/<id>.toml：仓库、tag、资产名、许可证、简介）
   ▼
CI（PR）
  1. schema 校验提交文件
  2. 按 §6 拼出每个平台的精确 URL
  3. 下载每个 zip → 校验 SHA256SUMS / sha256
  4. 解压（限额内）→ 校验 plugin.json：id / version / abi / entry / platforms
  5. 与 tag 比对 version；与文件名比对 id
  6. 干跑生成 index 片段，贴进 PR 评论
   ▼
合并 main → publish.yml
  1. 重新拉全部资产（不复用 PR 的缓存，防「审的和发的不一样」）
  2. 生成 index.json
  3. （可选）minisign 签名 index.json → index.json.minisig
  4. 发布到 gh-pages
```

### 10.4 签名（P1）

- 索引签名（`index.json.minisig`）比资产签名更重要：**索引能改，就能把任何资产换成自己的**；
- 公钥由宿主 / 商店**预置**，配置可覆盖但**不从索引本身取**；
- **P0 不校签名**：TUI 商店只做 sha256 + 「此包未签名」提示（[§13.3](#133-tui-商店的依赖与签名)）；
- 签名保护的是「下载」这一步，不是「加载」这一步——宿主加载本地目录时不需要它。

---

## 11. 镜像 / 离线 / 可复现

| 需求 | 谁负责 | 做法 | 注意 |
| --- | --- | --- | --- |
| 镜像 | TUI 商店（P0 用 `registry_url`；前缀改写属 P1） | 把 `[plugins].registry_url` 指到镜像的 `index.json` | 索引与资产必须**同源**；只改资产等于让镜像能换字节 |
| 离线 | 用户 / TUI 商店 | 手动把 zip 拷进来 → 解压到 `plugins/<id>/` → 重启；P0 商店没有落盘索引，离线时只显示错误与已装插件 | 不需要任何命令；解压工具不限 |
| 可复现 | 外部安装器（可选） | 按 `plugins.lock` 里的 sha256 取，不按 `latest` | 本仓不读 lock；没有 lock 时至少把 sha256 显示给用户 |
| 降级 | TUI 商店 | 索引拉不到 → 显示错误 + 已装插件，标「商店不可用」；**不影响 `run`** | 别把「商店不可用」升级成「bot 不能启动」 |

安全提醒：在签名（[§6.4](#64-可选-minisign)）落地之前，「索引 + 资产都走镜像」等价于**完全信任镜像**。
要么接受这一点，要么先做索引签名。

---

## 12. TUI 商店（分发 P0）

> **位置：`crates/fuyubot-cli/src/panel/store.rs`（单文件模块）。**
> 不新建 crate、不进 `fuyubot-loader`、不进运行时。这是整个仓库里**唯一**允许出现
> 「HTTP」和「zip」两个词的地方。
>
> 入口：`fuyubot run --tui`（`-t`）→ 商店页。面板本身不接 TDLib、不加载插件（既有约定）。

### 12.1 职责边界：四层，不许串味

| 层 | 能做 | 绝对不能做 |
| --- | --- | --- |
| `fuyubot-loader` / 运行时 | 读本地 `plugins/<id>/`、`probe`、`dlopen`、激活、回滚 | 网络、zip、解压、写 `plugins/`（`install`/`migrate` 的复制除外） |
| CLI 子命令（`plugin list/install/uninstall/migrate`） | 本地目录复制、删目录、迁移 | zip、下载 |
| **TUI 商店（`panel/store.rs`）** | 拉索引、搜索、详情、下载、sha256、解压、安装 / 卸载 | 改 ABI、改加载语义、把「zip」这个概念漏给 loader |
| 外部商城后端（分发 P1） | 索引托管、收录审核、站点 | 决定本仓怎么加载插件 |

一句话：**网络与解压只活在 `panel/store.rs`；`fuyubot-loader` 永远不知道有网络这回事。**

### 12.2 功能清单

| 功能 | 做什么 | 细节 |
| --- | --- | --- |
| 拉索引 | GET `index.json` | 进商店页拉一次、`r` 重拉；失败保留旧索引并显示「r 重试」（P0 不落盘缓存、不做 ETag/TTL） |
| 搜索 | 本地内存过滤 `id` / `name` / `tags` / `summary` | **不做服务端搜索**，索引就那么大 |
| 详情 | 版本、平台、大小、sha256、依赖、许可证、主页、release 链接 | 全部来自索引条目 |
| 安装 | 下载 → sha256 → staging 解压 → 校验 manifest → 落盘 | 目标已存在就报错（先卸载），不覆盖；见 [§12.3](#123-安装流水线) |
| 更新 | **P0 不支持原地更新**：先卸载再装（卸载会删目录，数据在 `data/`/配置里不受影响） | 只提示有新版本，**不自动更新** |
| 卸载 | 删 `plugins/<id>/` | 删已映射的原生 dll 需先停实例（[§12.5](#125-与实例状态的关系)） |

### 12.3 安装流水线

```text
选中条目（<id>@<version>@<platform>）
  ▼
download   精确 URL 取 zip 进内存（≤50MB，分块读、报 5% 进度；不落临时文件）
  ▼
verify     sha256（必做）——不过就停，显示期望/实际；P0 不校签名，也不显示签名状态
  ▼
stage      解压到 plugins/.staging/<id>/，边解边查限额（§7.3）
  ▼
validate   BundleManifest 校验：id / version / entry 存在 / platforms 含本机 / abi 与宿主匹配
  ▼
swap       同卷 rename：staging → plugins/<id>（或 plugins/scripts/<id>）
  ▼
cleanup    清 .staging/<id>（成功时已经被 rename 走；失败也清）
  ▼
refresh    重扫本地插件列表，提示「重启生效」
```

与 [§8.3](#83-安装流水线tui-商店与外部安装器共用) 是**同一条流水线**：TUI 商店是本仓的实现，外部安装器可以照抄。
两边必须遵守同一套约束（同卷 rename、id 决定目录名、不合并、目标已存在就报错）。

### 12.4 依赖账（已定）

| 需要 | 现状 | 结论 |
| --- | --- | --- |
| HTTP | `ureq` 已在 `[workspace.dependencies]`（原先只被 `fuyubot-tdlib` 当 build-dependency） | ✅ 已加 `fuyubot-cli` 依赖 |
| sha256 | `sha2` 已在 workspace（原先只服务 vendored） | ✅ 已加 `fuyubot-cli` 依赖 |
| zip 解压 | 原 workspace 没有 | ✅ 新增 `zip = "4.6.1"`（`deflate-flate2-zlib-rs`，纯 Rust 后端） |
| 签名 | 无 | **P1**；P0 只做 sha256（索引没有签名字段，不显示签名状态）（[§13.3](#133-tui-商店的依赖与签名)） |

`AGENTS.md` 约束 9 已补例外：`fuyubot-cli` 的 TUI 商店可以依赖 `ureq` / `sha2` / `zip`，
它们不在 `fuyubot-tdlib` 的 normal 树里，不违反「运行时依赖图」那条。

### 12.5 与实例状态的关系

- **不一律拒绝**：装一个新插件目录、卸一个脚本插件目录，实例在跑也允许——只是**原生插件要重启才生效**；
- 唯一真会失败的是 **Windows 上删 / 替换正在映射的 dll**（`拒绝访问` / sharing violation）。
  这种情况按报错提示「先 `fuyubot stop`」即可，不要预先一刀切拒绝所有操作；
- TUI 自己 `spawn` 的 `fuyubot run --background` 同样算「在跑」；
- 安装成功后**不自动重启**实例：提示「重启生效」。Go 插件更是只能重启（[§4.3](#43-go为什么必须拒绝为什么最后加载)）。

### 12.6 安全要求（和外部安装器同一套）

1. sha256 不匹配 → 拒绝安装，显示**期望值与实际值**（别只说「校验失败」）；
2. P0 索引没有签名字段：**不显示「已签名」、也不假装校验过**；信任链（minisign）等 P1，
3. zip 限额、路径穿越、符号链接、大小写折叠查重全部按 [§7.3](#73-限额与契约一致) / [§7.4](#74-解压实现要求tui-商店与外部安装器通用) 执行；
4. 只写 `plugins/.staging/`、`plugins/<id>/`、`plugins/scripts/<id>/` 三个位置，别的地方一个字节都不碰；
5. 失败一律清 staging，不留半个插件目录。

### 12.7 配置（分发 P0）

索引地址是**平铺在 `[plugins]` 里**的一个键，与 `dir` / `log_level` 同级：

```toml
[plugins]
dir = "plugins"
log_level = "info"
# 商店索引地址。**不写就是未配置**：商店页会显示去哪配，不会自己编一个默认地址。
# 环境变量 FUYUBOT_REGISTRY_URL 优先于这里（CI / 内网镜像用）。
registry_url = "https://catmoecircle.github.io/FuyuBot-plugins/index.json"
```

- 解析顺序：`FUYUBOT_REGISTRY_URL` → `[plugins].registry_url` → **未配置**（商店页空态 + 配置指引）；
  官方商城地址定下来之前不写死默认值，避免把用户导到一个不存在的域名。
- **`[plugins.store]` 这种嵌套段属于 P1**（镜像 / TTL / 强制签名），P0 不实现，别照着写配置；
- P0 想做镜像就改 `registry_url` 指向镜像的 `index.json`——索引与资产同源，
  顺带避开「只换资产、不换索引」那个信任问题（[§11](#11-镜像--离线--可复现)）。

### 12.8 非目标

- 不做评分 / 评论 / 账号；
- 不做自动更新（只提示）；
- 不做增量 / 差分更新；
- 不在 TUI 里跑插件、不改插件配置（配置仍归 `fuyubot.toml`）；
- 不做「商城后端」——那是分发 P1、独立仓库。

### 12.9 其它宿主

`fuyubot-android` 只交付 `libfuyubotjni.so`，不出 APK / UI（`AGENTS.md`）。
APK 作者要商店就自己实现一个，**消费同一份 bundle 契约**即可；
本仓不提供、也不要求任何宿主复用 `panel/store.rs`。

---

## 13. 需要拍板的点

### 13.1 ABI 兼容策略

**现状**：`load_with_config` / `probe` 里都是 `handle.abi_version != ABI_VERSION` → `AbiMismatch`，
**精确相等**。`ABI_VERSION = 5`。

**为什么商店会放大代价**：每次 ABI bump，索引里所有插件的 `abi` 全部过期，
用户升级宿主后看到的是「昨天还能用的插件今天全红」，而且没有任何平滑路径。

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| **A 维持精确相等**（现状） | 索引按 `abi` 分版本；宿主升级时提示「这些插件需要新版」 | 每次 bump = 全量重编 + 索引重发；用户可感知的断裂 |
| **B 大版本 + 长度协商** | `ABI_VERSION` 拆 `(major, minor)`；VTable 头加 `size`；minor 只追加尾部字段，宿主按 `size` 取交集 | 要改 `HostApiVTable` 布局（**这本身就是破坏性变更**，必须挑一次大版本做，v6）；所有插件重编一次 |
| **C 宿主带 N-1 兼容层** | 宿主同时支持 v5 / v6 两套桥 | 桥接代码翻倍、长期不可持续；只在「关键插件」场景值得 |

**建议**：现在走 A（并且把 `abi` 写进索引、安装前预检），把 B 排进 v6 的设计里**一次做完**。
不要试图「悄悄地」让 v5 插件跑在 v6 宿主上——VTable 大小变了，跑起来是 UB，不是报错。

**同时要拍板**：索引是否按宿主 ABI 分目录（`/v5/index.json`、`/v6/index.json`）。
分的话老宿主永远能拉到能用的插件；不分就得靠客户端过滤，容易漏。

### 13.2 目录统一时机

现状：原生 `plugins/<id>/`，脚本 `plugins/scripts/<id>/`。

| 选项 | 做法 | 代价 |
| --- | --- | --- |
| A 永不统一 | `scripts/` 作为脚本的稳定命名空间 | 两套扫描逻辑长期并存；zip 布局分两种 |
| B 将来统一（未排期） | 全部 `plugins/<id>/`，用 `kind` 区分 | 要改 SDK 中间插件的扫描根（跨仓），且 `AGENTS.md` 约束 51 的发布顺序要跟着动 |
| C 宿主指定扫描根 | 中间插件从宿主配置读扫描根，目录形态随宿主 | 契约变更（中间插件要认一个新配置键），但比 B 轻 |

**建议**：分发 P0/P1 都不动（选 A 的现状），等中间插件支持「宿主指定扫描根」（C）之后再谈 B。
真正该先做的是「让 `plugin list` 把两种形态都列清楚」，而不是搬家。

### 13.3 TUI 商店的依赖与签名

- **依赖（已定）**：`fuyubot-cli` 已加 `ureq` / `sha2` / `zip`（[§12.4](#124-依赖账已定)），
  CLI 是唯一带网络与解压能力的 crate；`AGENTS.md` 约束 9 已补例外。
- **签名**：P0 索引没有签名字段，**不显示签名状态**、也不假装校验过；minisign 等 P1。
- **索引可信度**：索引由商城 CI 生成；如果索引本身被换掉，sha256 也跟着换——所以
  索引签名比资产签名更重要。这也是「签名等 P1」的前提：P1 之前，索引必须从**固定官方 URL** 拉。

### 13.4 静态 `provides` 排序是否值得做

- 收益：不 `dlopen` 就能排出加载顺序，并在**安装时**发现「依赖的服务没人提供」；
- 代价：`provides` 是作者声明，可能与实际 `provide_service` 不一致（声明了没提供 → 排序对了也起不来）；
  还要定义「缺提供者」怎么报（建议：**硬依赖缺提供者 = 安装时警告**；启动时按现有内核行为处理——
  `inject` 缺服务就是停在 Waiting 不激活，不要为此新增一种致命错误）。

### 13.5 zip 的入口只有一个

契约（`docs/plugin-format-v1.md` §6）已按这个口径改好：**CLI 的 `plugin install` 不处理 zip；
zip 只由 TUI 商店消费（或用户手动解压）**。这样「谁能吃 zip」只有一个答案，
不会出现「CLI 一个半吊子解压 + TUI 一个完整解压」。

### 13.6 `plugins.lock` 归属

本文把它划给安装器（[§8.5](#85-可选账本-pluginslock不在本仓)）。如果将来希望宿主也能「按锁文件校验已装插件」，
那是一条新的本仓增量（只读校验，不做安装），需要单独排期。

### 13.7 卸载是否保留数据

建议：`uninstall` **只删插件目录**，保留 `[plugin.<id>]` 配置与 `data/plugins/<id>/`；
另给 `--purge` 显式删数据（并二次确认）。现在的 `uninstall` 已经是「只删目录」，保持一致。

### 13.8 其它小项

| 项 | 建议 |
| --- | --- |
| `id` 是否强制反向域名 | 本仓不强制（本地插件随便叫）；**商城收录强制** |
| `version` 是否强制 semver | 本仓 P0 不强制；商城强制（要排序、要判新版） |
| 一个 zip 能否装多个插件 | **禁止**：一 zip 一插件。否则 `id` / 目标目录 / 原子替换 / 锁文件全部要重设计 |
| `abi` 缺失时是否拒绝安装 | 不拒绝，`plugin list` 显示 `?`，真值加载时判定 |
| 是否允许插件自带 `bin/` 之外的 dll | 允许（兄弟库），但**只 `dlopen` `entry` 那一个**；兄弟库靠 `open_library` 的同目录查找（约束 30） |

---

## 附录 A：本仓落点

| 位置 | 职责 | 状态 |
| --- | --- | --- |
| `crates/fuyubot-abi/src/lib.rs` | `ABI_VERSION = 5`、`SYM_*` 常量、`HostApiVTable` | ✅ 不动 |
| `crates/fuyubot-loader/src/bundle.rs` | `BundleManifest` / `BundleError` / `plugin_entries` / `script_bundles` / `disabled_bundles` / `current_platform` | ✅ 落地 |
| `crates/fuyubot-loader/src/loader.rs` | `probe` / `probe_bundle` / `probe_entry` / `DylibPlugin` / `plugin_files` / `Probe` | ✅ 扩展完成 |
| `crates/fuyubot-loader/src/lib.rs` | `pub use` 新公开面 | ✅ |
| `crates/fuyubot-app/src/app.rs` | 加载队列、Go 分区（`unloadable`）、`probe_entry` 调用 | ✅ 新 API 接线完成 |
| `crates/fuyubot-cli/src/scaffold.rs` | `install` / `install_script_dir` / `uninstall` / `install_builtins` / `find_artifact` / `script_metadata` / `migrate` | ✅ 目录化改造完成 |
| `crates/fuyubot-cli/src/main.rs` | `plugin list`（去 `dlopen`）/ `plugin migrate` | ✅ |
| `crates/fuyubot-cli/src/panel/store.rs` | TUI 商店：索引 / 搜索 / 详情 / 下载 / sha256 / 解压 / 安装 / 卸载（**唯一**允许出现 HTTP 与 zip 的模块） | ✅ 落地（21 条单测） |

新增依赖：**加载器与运行时零新增**（`serde_json` 已有）；TUI 商店要 `ureq` / `sha2` / `zip`，
只加在 `fuyubot-cli`（[§12.4](#124-依赖账已定)）。
zip / sha256 / HTTP 只出现在 `panel/store.rs`（以及外部安装器）里，
`fuyubot-loader` 与运行时的依赖图**一个都不沾**——这也是「边界划在模块级」的实际好处。

---

## 相关文档

- [`docs/plugin-format-v1.md`](plugin-format-v1.md)：分发 P0 的**权威契约**（本文与它冲突时以它为准）
- [`docs/plugin-migration.md`](plugin-migration.md)：用户与作者的迁移指南
- [`docs/design-script-plugin-v2.md`](design-script-plugin-v2.md)：脚本插件（中间插件 + 一脚本一进程）
- [`AGENTS.md`](../AGENTS.md)：约束 9 / 18 / 30 / 49 / 50 / 52 / 53 / 54 / 57 / **59 / 60 / 61** 与本文直接相关
