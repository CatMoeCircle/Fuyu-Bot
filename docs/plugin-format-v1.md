# 插件分发格式 v1（P0 契约）

> 状态：**已落地**（2026-11-03）。这份文件是并行开发的**接口契约**：
> `fuyubot-loader` 与 `fuyubot-cli` 两侧按它实现，任何偏离先改这份文件。
>
> 非目标（**加载器 / 运行时**不做）：商店索引 / 网络下载 / zip 解压 / 签名 / `plugins.lock`。
> 这些只在 CLI 的 TUI 商店（`crates/fuyubot-cli/src/panel/store.rs`）里；商城后端属于 P1，
> 见 `plugin-store-design.md`。

## 1. 目标

1. 一个插件 = 一个目录，目录里有 `plugin.json`（静态元数据）。
2. 宿主**不 dlopen** 就能列举、校验、排序插件。
3. 旧形态（平铺 `*.dll` / `plugins/scripts/<name>/`）继续可用，给 `plugin migrate` 迁移。
4. **ABI 不变**（仍是 v5，四个导出符号与 VTable 一个都不动）。

## 2. 目录布局

```text
plugins/
├── <id>/                       # 原生插件包（P0 的新形态）
│   ├── plugin.json
│   ├── bin/                    # 动态库 + 它的兄弟库
│   │   └── fuyubot_plugin_x.dll
│   ├── config.default.toml     # 可选
│   └── README.md / LICENSE     # 可选
├── scripts/                    # 脚本插件根（P0 不动，兼容 SDK 中间插件）
│   └── <id>/
│       ├── plugin.json         # 新增、可选：宿主只读它做静态列举
│       ├── package.json        # 或 pyproject.toml
│       └── ...
├── .disabled/<id>/             # 被禁用的包（不在扫描路径里）
├── weather.dll                 # 旧形态：平铺原生插件（兼容期）
├── weather.dll.disable         # 旧形态：被禁用的平铺插件
└── …
```

规则：

- 原生包扫描 `plugins/*/plugin.json`（**只下一层**，不递归）。
- 脚本包扫描 `plugins/scripts/*/plugin.json`（同样只下一层），**只用于列举**；
  真正的发现/启停仍由 SDK 的 `fuyubot_python_host` / `fuyubot_js_host` 负责。
- 以 `.` 开头的目录（`.disabled` / `.staging` / `.cache`）一律跳过。
- 名字以 `.disable` 结尾的目录也跳过（旧面板把目录改名禁用留下的历史形态）。
- 目录名**必须等于** `plugin.json` 的 `id`；不一致时以 `id` 为准并告警。
- 禁用 = 整个目录挪到 `plugins/.disabled/<id>/`，不写第二份清单。

## 3. `plugin.json`

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
  "description": "…",
  "license": "MIT",
  "homepage": "…"
}
```

字段：

| 字段 | 必填 | 说明 |
|---|---|---|
| `schema` | 是 | 目前只认 `1`；更大 → `UnsupportedSchema` |
| `id` | 是 | `[A-Za-z0-9][A-Za-z0-9._-]*`，≤128，不含路径分隔符，不以 `.` 开头 |
| `name` | 是 | 展示名（可中文） |
| `version` | 是 | 非空；只允许 `[0-9A-Za-z.+-]`（不强制 semver 解析，商店阶段再严） |
| `kind` | 是 | `native` / `python` / `js` |
| `entry` | 是 | 相对包根；禁止绝对路径、`..`、反斜杠开头；解析后必须仍在包内且是文件 |
| `abi` | native 建议 | 静态预检用的**建议值**，真值以 `plugin_init` 返回的 handle 为准 |
| `platforms` | 否 | 空 = 全平台；`any` 也等于全平台；否则必须含 `current_platform()` |
| `depends` | 否 | 硬依赖（服务名，进内核 `inject`） |
| `optional_depends` | 否 | 软依赖（进 `soft_inject`） |
| `provides` | 否 | 本插件提供的服务名（静态预检/商店展示用） |
| `config` | 否 | 默认配置相对路径；P0 只解析不合并 |
| 其余 | 否 | 未知字段**忽略**（向前兼容） |

`current_platform()` 返回 `windows-x86_64` / `linux-x86_64` / `macos-aarch64` 这类串，
与发布资产命名一致。

## 4. `fuyubot-loader` 公开 API（新增）

新模块 `crates/fuyubot-loader/src/bundle.rs`，`lib.rs` 里 `pub use`。
类型/签名如下（可以加私有辅助，公开面不要漂）：

```rust
pub const BUNDLE_MANIFEST: &str = "plugin.json";
pub const DISABLED_DIR: &str = ".disabled";
pub const SCRIPTS_DIR: &str = "scripts";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind { Native, Python, Js }

impl PluginKind {
    pub fn as_str(self) -> &'static str;
    pub fn parse(text: &str) -> Option<Self>;
    pub fn is_script(self) -> bool; // python | js
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleManifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: PluginKind,
    pub entry: String,
    pub abi: Option<u32>,
    pub platforms: Vec<String>,
    pub depends: Vec<String>,
    pub optional_depends: Vec<String>,
    pub provides: Vec<String>,
    pub config: Option<String>,
    pub description: String,
    pub license: String,
    pub homepage: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("…")] Io { path: PathBuf, #[source] source: std::io::Error },
    #[error("…")] Json { path: PathBuf, #[source] source: serde_json::Error },
    #[error("…")] Missing(&'static str),
    #[error("…")] BadId(String),
    #[error("…")] BadVersion(String),
    #[error("…")] UnsupportedSchema(u32),
    #[error("…")] BadEntry(String),
    #[error("…")] EntryMissing(PathBuf),
    #[error("…")] EntryEscapes(PathBuf),
    #[error("…")] UnsupportedPlatform { want: Vec<String>, host: &'static str },
}

impl BundleManifest {
    pub fn parse(text: &str) -> Result<Self, BundleError>;      // 无 IO；path 字段用 "<memory>"
    pub fn load(dir: &Path) -> Result<Self, BundleError>;        // dir/plugin.json
    pub fn supports_host_platform(&self) -> bool;
    pub fn entry_path(&self, dir: &Path) -> Result<PathBuf, BundleError>;
    pub fn validate(&self, dir: &Path) -> Result<(), BundleError>; // load + entry_path + 平台
}

pub fn current_platform() -> &'static str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginEntry {
    Bundle(PathBuf),  // 目录
    Legacy(PathBuf),  // 平铺动态库
}

pub fn plugin_entries(dir: &Path) -> std::io::Result<Vec<PluginEntry>>;
pub fn disabled_bundles(dir: &Path) -> std::io::Result<Vec<PathBuf>>;
pub fn script_bundles(scripts_dir: &Path) -> std::io::Result<Vec<PathBuf>>; // 只列有 plugin.json 的
pub fn is_bundle_dir(path: &Path) -> bool;
```

`plugin_entries` 的顺序：**bundle 按 id 字典序在前，legacy 按文件名在后**；
Go 的“最后加载”由调用方（`fuyubot-app` / CLI）按 `Probe.go_runtime` 分区，不在这个函数里做。
目录不存在时返回 `Ok(vec![])` 还是 `Err`？**与现有 `plugin_files` 一致**：不存在返回 `Err`
（调用方本来就按错误处理）；空目录返回 `Ok(vec![])`。

## 5. Probe / 加载语义

```rust
pub struct Probe {
    pub path: PathBuf,              // 展示路径：bundle 目录，或 legacy dll
    pub name: String,               // 插件名（bundle = id）
    pub abi_version: u32,           // 静态包：sidecar 的 abi 或 0（未知）；legacy：真实值
    pub manifest: Option<Manifest>, // 依赖：bundle 用 depends/optional_depends 合成；legacy 读 plugin_manifest
    pub entry: PathBuf,             // 真正 dlopen 的文件
    pub kind: PluginKind,
    pub bundle: Option<BundleManifest>,
    pub go_runtime: bool,           // 文件魔数判定，不 dlopen
}
```

`PluginLoader` 新增：

```rust
pub fn probe_bundle(&self, dir: &Path) -> Result<Probe, LoadError>; // 绝不 dlopen
pub fn probe_entry(&self, entry: &PluginEntry) -> Result<Probe, LoadError>;
/// 只给 `plugin migrate` 用：允许探测 Go 的 c-shared（会 dlopen，进程即将退出）。
pub fn probe_for_migration(&self, path: &Path) -> Result<Probe, LoadError>;
```

- `probe_bundle`：读 `plugin.json` → 校验 → `entry_path` → 读文件魔数判 Go →
  返回 `Probe`。**不调用 `open_library` / `plugin_init`**。
- `probe`（旧）：先 `looks_like_go_binary`，**是 Go 就返回新错误
  `LoadError::GoRequiresManifest { path }`，不 dlopen**；否则维持现状。
- `DylibPlugin::probe(loader, path, config)`：`path.is_dir()` → `probe_bundle`，否则 → `probe`。
- `DylibPlugin::apply` 用 `self.entry`（不是 `self.path`）去 `start_library`。
- `DylibPlugin::abi_version()` 返回 `Probe.abi_version`（bundle 可能是 0，表示“装完才知道”）。

`LoadError` 新增：

```rust
#[error("Go 插件 {path} 没有 plugin.json：它无法卸载，必须先有静态元数据才能安全加载")]
GoRequiresManifest { path: PathBuf },
```

`scan_dir` / `load_dir` 改为走 `plugin_entries` + `probe_entry`，保持现有返回类型。

## 6. CLI 行为

### `fuyubot plugin list`

**不再 dlopen / 不再创建 Kernel。** 输出四段：

1. 原生包：`<id>\t<version>\tABI <n|?>\t<kind>\t<path>`
2. 旧形态 dll：`<文件名>\tlegacy\t<path>`（读不了元数据，直说）
3. 脚本包：`scripts/<dir>` 有 `plugin.json` 就按它列；没有就按 `package.json` / `pyproject.toml` 列
4. 禁用项：`.disabled/<id>/` 与 `*.disable`

### `fuyubot plugin install <路径>`

- 目录 + `plugin.json`：
  - `kind = native` → 复制到 `plugins/<id>/`
  - `kind = python/js` → 复制到 `plugins/scripts/<id>/`
  - 目标存在 → 报错（不覆盖）
- 目录无 `plugin.json` 但 `is_script_plugin_dir` → 旧行为（`scripts/<目录名>/`），
  顺手把从 package.json/pyproject 生成的 `plugin.json` 写进目标（失败不致命）。
- 文件 dll → 旧行为；**Go 且无 sidecar → 报错并指路 `plugin migrate`**。
- 其它（zip / tar 等归档）→ CLI **不处理**，明确报错并指路：
  「用 TUI 插件商店安装，或手动解压到 `plugins/<id>/`」。

### `fuyubot plugin uninstall <名字>`

按顺序找：`plugins/<id>/` → `plugins/scripts/<id>/` → `.disabled/` 下两处 →
旧形态 dll（带不带 `lib` 前缀、带不带 `.disable`）。找到就整个目录删。

### `fuyubot plugin migrate [--dry-run]`

- 平铺 dll：**这里允许 dlopen 一次**（用户显式触发，进程即将退出）拿
  name/version/deps → 建 `plugins/<id>/bin/` → 移动 dll + 同主干同伴脚本
  （`.py/.js/.ts/.pyi`）→ 写 `plugin.json`。
  - `id` 取 `plugin_init` 的名字；非法 id（含 `/` 等）→ 报错跳过。
  - 目标已存在 → 报错跳过，不合并。
  - Go：允许，但打一条「已永久映射，仅本次迁移进程」的说明。
- `scripts/<dir>` 无 `plugin.json`：从 package.json / pyproject.toml 合成
  `id/name/version/kind/entry` 写入（entry 默认 `index.ts`→`index.js` / `main.py`），
  不移动目录。
- `--dry-run` 只打印计划。
- 幂等：已有 `plugin.json` 的直接跳过。

## 7. 商城索引（TUI 商店消费的契约）

索引 URL 来自 `fuyubot.toml` 的 `[plugins].registry_url`（或环境变量
`FUYUBOT_REGISTRY_URL`）。索引是**静态 JSON**，由外部商城托管；本仓只消费，不生成。

```json
{
  "schema": 1,
  "plugins": [{
    "id": "com.example.weather",
    "name": "Weather",
    "summary": "一句话",
    "description": "长描述（可空）",
    "repo": "https://github.com/owner/repo",
    "license": "MIT",
    "homepage": "https://…",
    "tags": ["weather"],
    "latest": "1.2.0",
    "versions": [{
      "version": "1.2.0",
      "api": ">=1.0, <2.0",
      "abi": 5,
      "depends": ["fuyubot.help.commands"],
      "yanked": false,
      "assets": {
        "windows-x86_64": { "url": "https://…zip", "sha256": "hex", "size": 1234567 },
        "any": { "url": "https://…zip", "sha256": "hex", "size": 234567 }
      }
    }]
  }]
}
```

规则：

- `schema` 只认 `1`；未知字段忽略。
- 资产按 `current_platform()` 精确匹配，找不到再取 `any`；都没有 = 该平台不可用。
- `sha256` **必填**（大小写不敏感）；`size` 可选，只做显示/预检。
- `yanked: true` 的版本不参与选择。
- `api` 是**宿主 `fuyubot` 的语义版本区间**（不是 ABI、也不是 plugin-api 的版本）；
  P0 的 TUI 商店只做展示/提示，**不**据此拒绝安装——真正的兼容性以 `abi` 与运行时校验为准。
- `abi` 大于宿主 `ABI_VERSION` → 明确报「需要更新的宿主」，不下载。
- 下载 URL 只允许 `https`；解压时必须防 zip-slip / 符号链接 / zip bomb
  （单包 ≤ 50MB、解压后 ≤ 200MB、文件数 ≤ 2000）。
- 解压后必须用 `BundleManifest` 再校验一次 `id` / `version` / `entry` / 平台。

## 8. 约束

- **不改 ABI**：`fuyubot-abi` 一行不动。
- **不递归扫描**：`node_modules` / `.venv` 绝不能进扫描。
- **不改脚本插件根**：`plugins/scripts/` 保持，SDK 中间插件不动。
- 路径一律 `Path`，不拼字符串；Windows 反斜杠要兼容。
- 测试临时目录用 `target/test-tmp/...`（仓库约定），不要用系统 TEMP。
- 所有新增公开项要有 rustdoc；错误信息要能指导用户下一步。
- 改动后至少 `cargo test -p fuyubot-loader --offline` /
  `cargo test -p fuyubot-cli --offline` / `cargo fmt --all --check` 通过。
