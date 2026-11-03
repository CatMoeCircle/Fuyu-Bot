# 插件迁移指南：旧形态 → bundle

> 读者：**用插件的人**（第 2、4、5、6 节）与**写插件的人**（第 3 节）。
> 状态：分发 P0 的格式契约已冻结（[`docs/plugin-format-v1.md`](plugin-format-v1.md)），
> 代码在并行落地。**命令以 `fuyubot plugin --help` 的实际输出为准**——
> 如果你敲 `fuyubot plugin migrate` 得到「未知子命令」，说明手上的版本还没带上它，
> 按 [§2.7](#27-手工迁移命令不可用时) 手工做，效果一样。
>
> 装**新**插件有两条路，迁移只处理「已经躺在 `plugins/` 里的旧形态」：
>
> 1. **TUI 商店**（`fuyubot run --tui` → 商店页）：搜索、看详情、下载、sha256 校验、解压、安装一条龙（分发 P0）；
> 2. **手动**：从作者 release 或商城下载 zip → 解压到 `plugins/<id>/` → 重启。

---

## 1. 一页对照

| | 旧形态 | 新形态（bundle） |
| --- | --- | --- |
| 原生插件 | `plugins/weather.dll`（平铺） | `plugins/com.example.weather/`<br>`├── plugin.json`<br>`└── bin/weather.dll` |
| 脚本插件 | `plugins/scripts/weather-bot/` | **分发 P0 不变**；可加一个可选的 `plugin.json` 供 `plugin list` 展示 |
| 身份从哪来 | `dlopen` 后问 `plugin_init` | `plugin.json` 的 `id`（静态可读） |
| 探测会不会执行插件代码 | **会**（必须 `dlopen` 才知道是谁） | **不会**（读 JSON + 文件魔数） |
| 依赖声明 | `plugin_manifest` 导出，或 `[plugin.<名>].inject` | `plugin.json` 的 `depends` / `optional_depends`（合成进内核 `inject`） |
| 禁用 | 改名 `weather.dll` → `weather.dll.disable` | 整个目录挪进 `plugins/.disabled/com.example.weather/` |
| 卸载 | 删 dll + 同名同伴脚本 | 删整个目录 |
| Go 插件 | 能加载，但**永不卸载**、且探测即永久映射 | **必须有 `plugin.json`**，否则直接拒绝（`GoRequiresManifest`） |
| 装一个插件 | 拷 dll 进 `plugins/` | TUI 商店一键装；或下载 zip 解压到 `plugins/<id>/`；或 `fuyubot plugin install <目录>` |
| zip 谁来解压 | — | TUI 商店（内置下载 + 校验 + 解压）；CLI 子命令**永远不吃 zip** |
| 多平台 | 一个 dll 一个平台，靠文件名猜 | `plugin.json` 的 `platforms` 显式声明 |

一句话：**从「一堆文件」变成「一个带身份证的目录」**。宿主不执行代码就能知道它是谁、要什么、能不能在这台机器上跑。

---

## 2. 用户迁移

### 2.1 动手前：停机 + 备份

```powershell
fuyubot stop                       # 必须：Windows 上被映射的 dll 连改名都做不到
Copy-Item -Recurse plugins plugins.bak-20261103
```

- **一定要停**。宿主机在跑的时候，`migrate` 移动 dll 会失败（`拒绝访问` / sharing violation），
  而且失败信息看起来像「权限问题」，很容易误判。
- 备份整个 `plugins/` 是最省事的回滚手段（[§2.6](#26-回滚)）。
- 顺手记一下现在能用的插件：`fuyubot plugin list`。

### 2.2 看计划：`--dry-run`

```powershell
fuyubot plugin migrate --dry-run
```

它**只打印计划、不动文件**。示例（`[dry-run]` 前缀是实际格式）：

```text
[dry-run] 会把 plugins\weather.dll 迁到 plugins\com.example.weather/（kind=native，ABI 5，依赖：无）
[dry-run] 会把 plugins\counter.dll 迁到 plugins\fuyubot.example.counter/（kind=native，ABI 5，依赖：fuyubot.help.commands）
[dry-run] 会给 plugins\scripts\echo-bot 写 plugin.json（id=fuyubot.example.echo，kind=js，entry=index.ts）
跳过内置插件 plugins\fuyubot_plugin_start.dll（run 会自己装；迁成包会导致重复加载）
跳过 plugins\bad name.dll：插件 id "bad name" 不合法：…
```

看三件事：

1. **id 对不对**（迁移后的目录名、`[plugin.<id>]` 配置段名都跟着它走）；
2. **同伴文件有没有带上**（`.py` / `.js` / `.ts` / `.pyi`）；
3. **有没有「跳过」**，以及原因。

### 2.3 执行

```powershell
fuyubot plugin migrate
```

逐项进行，**一个失败不影响其它**；每项打印一行「已迁移 / 跳过 / 失败」。

它对每个平铺 dll 做的事：

```text
dlopen 一次（用户显式触发，进程马上退出）
  → 读 name / version / deps
  → 建 plugins/<id>/bin/
  → 移动 dll + 同主干同伴脚本（.py/.js/.ts/.pyi）
  → 写 plugins/<id>/plugin.json
```

对 `plugins/scripts/<目录>/`：只**补一个 `plugin.json`**（从 `package.json` / `pyproject.toml`
合成 `id` / `name` / `version` / `kind` / `entry`），**不移动目录**。

对 Go 插件：允许迁移，但会打一条说明——它一旦被映射就永久留在进程里，迁移只是把「元数据」补上。

### 2.4 复核 + 重启

```powershell
fuyubot plugin list        # 应该看到原生包（带 id / version / ABI）而不是「legacy」
fuyubot run
```

启动日志里每个插件应该有一条加载记录；`跳过插件 <路径>：<原因>` 是要处理的。

以后装**新**插件就不用再碰文件了：`fuyubot run --tui` → 商店页，搜索 → 详情 → 安装。
迁移只是把**已有的**旧形态整理成 bundle，两者互不影响。

### 2.5 失败怎么办

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `拒绝访问` / sharing violation | 宿主还在跑，dll 被映射 | `fuyubot stop` 后重跑 |
| `id 非法` 跳过 | `plugin_init` 的名字含 `/`、空格、或以 `.` 开头 | `id` 来自 `plugin_init`，改名得作者改代码——**不能靠改目录名绕过**（改了会变成「名字对不上」直接拒绝加载）。这类插件只能继续用旧形态 |
| `目标已存在` 跳过 | `plugins/<id>/` 已经有东西（迁移过一次 / 手工建过） | 不合并是故意的。确认旧的那份没用后，删掉目标目录再重跑 |
| 迁移成功但启动报 `EntryMissing` | 产物名与 `entry` 不一致（Linux 上忘了 `lib` 前缀） | 改 `plugin.json` 的 `entry` |
| 迁移成功但命令回了两次 | 平铺 dll 没删干净，legacy + bundle 各加载一次 | 删掉 `plugins/` 下那个平铺 dll |
| `plugin.json` 写出来但依赖丢了 | 老插件没有 `plugin_manifest`（ABI v3 及以前） | 手工把 `depends` 补进 `plugin.json` |

失败项**保留原样**（平铺 dll 不动），所以「迁移了一半」也能照常启动——只是那部分还是旧形态。

### 2.6 回滚

迁移是「移动」，所以回滚就是反着移：

```powershell
# 单个插件
Move-Item plugins\com.example.weather\bin\weather.dll plugins\weather.dll
Remove-Item -Recurse plugins\com.example.weather

# 或者最稳的：整目录恢复
Remove-Item -Recurse plugins
Rename-Item plugins.bak-20261103 plugins
```

注意：**先停宿主**再动，理由同 [§2.1](#21-动手前停机--备份)。

### 2.7 手工迁移（命令不可用时）

和 `migrate` 做的一样，只是手动：

1. 停宿主；
2. 建 `plugins\<id>\bin\`，把 dll（和同伴 `.py` / `.js` / `.ts` / `.pyi`）移进去；
3. 在 `plugins\<id>\plugin.json` 写：

```json
{
  "schema": 1,
  "id": "<id>",
  "name": "<展示名>",
  "version": "1.2.0",
  "kind": "native",
  "entry": "bin/weather.dll",
  "abi": 5,
  "platforms": ["windows-x86_64"]
}
```

4. **删掉平铺的那份 dll**（不删会被加载两次）；
5. `fuyubot plugin list` 复核 → `fuyubot run`。

`<id>` 用插件自报的名字（就是 `[plugin.<名>]` 那个名字），不要用文件名。

---

## 3. 作者迁移：给插件补 `plugin.json`

### 3.1 通用规则（五种语言都适用）

| 字段 | 怎么填 |
| --- | --- |
| `id` | 插件在宿主里的名字（`define_plugin!` / `plugin_init` 报的那个），**不是**文件名、**不是** crate 名；**必须与 `plugin_init` 报的名字完全一致**，否则拒绝加载 |
| `entry` | 相对包根的**真实文件**路径，必须在 `bin/` 下；大小写、`lib` 前缀都要对 |
| `abi` | `5`（当前 ABI）；它只是预检，真值以 `plugin_init` 返回的 handle 为准 |
| `depends` | **服务名**（内核 `inject` 用的名字，如 `fuyubot.help.commands`），不是插件名 |
| `optional_depends` | 软依赖：缺了也能起，有了优先排前面 |
| `provides` | 你 `provide_service` 出来的服务名（目前只用于列举 / 商城展示） |
| `platforms` | 你实际编了哪些平台就写哪些；空 = 全平台（**多平台产物不要写空**） |
| `config` | 有默认配置就写 `config.default.toml`（分发 P0 只解析，不合并） |

### 3.2 Rust

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
  "depends": ["fuyubot.help.commands"]
}
```

- Windows 产物：`fuyubot_plugin_weather.dll`；Linux / macOS：`libfuyubot_plugin_weather.so` / `.dylib`
  ——**`entry` 要写平台对应的名字**，所以「一个 `plugin.json` 通吃全平台」只在产物同名时成立，
  否则一个平台一个包（[§3.6](#36-打-zip发布用)）。
- `version` 与 `Cargo.toml` 的 `version` 保持一致：`plugin_manifest` 报的是 `CARGO_PKG_VERSION`。
  对不上时不会拒绝加载，但 `plugin list` / 商店按 `plugin.json` 展示，日志里那份会显得对不上。
- 依赖本来就写在 `define_plugin!` 的 `inject` 里？可以继续留着。bundle 里以 `plugin.json` 的
  `depends` 为准（宿主把它合成进内核 `inject`，**覆盖** `[plugin.<名>].inject`，逐侧判定，约束 53）。
  两份写一样最省心。

### 3.3 Go

```json
{
  "schema": 1,
  "id": "com.example.gocounter",
  "name": "Go Counter",
  "version": "0.3.0",
  "kind": "native",
  "entry": "bin/gocounter.dll",
  "abi": 5,
  "platforms": ["windows-x86_64"]
}
```

**Go 是唯一「没有 `plugin.json` 就完全不能加载」的语言**，原因是硬的：

- c-shared 的 Go runtime 起自己的线程，**不能 `dlclose`**，所以宿主识别出 Go 就故意不卸载（`AGENTS.md` 约束 50）；
- 于是「先 `dlopen` 看看它是谁」对 Go 是**不可逆**的：一次探测 = 永久映射；
- 所以顺序只能是「先有静态元数据，才允许加载」。

宿主给的错误会直接说明：

```text
Go 插件 plugins/gocounter.dll 没有 plugin.json：它无法卸载，必须先有静态元数据才能安全加载
```

另外两条要记住：

- **更新 Go 插件 = 重启进程**。同一进程里重复加载不可靠，别承诺热更新；
- `platforms` 一定要写：Go 产物是平台相关的，写空会被当成「全平台」，然后在别的平台上装上去起不来。

### 3.4 C / C++

四个导出符号（`plugin_init` / `plugin_start` / `plugin_stop` / `plugin_manifest`），
外加可选的 `plugin_free_string` / `plugin_free_handle`。`plugin.json` 与 Rust 完全一样：

```json
{
  "schema": 1,
  "id": "com.example.weather",
  "name": "Weather",
  "version": "1.0.0",
  "kind": "native",
  "entry": "bin/weather.dll",
  "abi": 5,
  "platforms": ["windows-x86_64"]
}
```

- 头文件是**产物**，权威副本是 `fuyubot_abi::PLUGIN_HEADER`；用 `fuyubot-sdk codegen --lang c --dir <工程>` 落盘，
  别从别的插件抄一份旧的。
- 手编时容易漏掉 `plugin_manifest`：没有它不影响加载，但依赖声明就只能靠 `[plugin.<名>].inject`，
  换台机器忘了抄就是**静默的加载顺序错误**。
- 兄弟库（`bin/` 里其它 dll）可以带，但宿主只 `dlopen` `entry` 那一个；
  兄弟库靠「库自己所在目录优先」的查找规则找到（`AGENTS.md` 约束 30），所以它们必须和 `entry` 同目录。

### 3.5 Python / JavaScript

分发 P0 **不动**脚本插件的家：仍然是 `plugins/scripts/<目录>/`。
`plugin.json` 是**可选**的，只用于 `plugin list` 静态展示——真正的发现与启停归 SDK 的两个中间插件，
它们看的是 `pyproject.toml` 的 `[tool.fuyubot]` / `package.json` 的 `fuyubot` 段。

Python（`plugins/scripts/weather-bot/plugin.json`）：

```json
{
  "schema": 1,
  "id": "com.example.weather",
  "name": "Weather",
  "version": "0.1.0",
  "kind": "python",
  "entry": "main.py",
  "platforms": ["any"]
}
```

JavaScript / TypeScript：

```json
{
  "schema": 1,
  "id": "com.example.weather",
  "name": "Weather",
  "version": "0.1.0",
  "kind": "js",
  "entry": "index.ts",
  "platforms": ["any"]
}
```

- `entry` 写**脚本**，不是 dll；`kind` 写 `python` / `js` 而不是 `native`。
- 不写 `plugin.json` 也能跑（`plugin list` 会退回 `package.json` / `pyproject.toml` 去列）。
- 目录里的 `node_modules/` / `.venv/` **不要**打进任何发布包：安装期由中间插件 `bun install` / `uv sync` 现装。

### 3.6 打 zip（发布用）

> zip 是**发布契约**（[`docs/plugin-store-design.md`](plugin-store-design.md#7-zip-布局与解压安全发布契约)）。
> 消费它的是 **TUI 商店**（本仓，`panel/store.rs`）和外部商城 / 第三方安装器；
> **加载器不读 zip**，只读解压后的目录。

规则三条：

1. **根 = 插件目录内容**，不套一层（`plugin.json` 必须在 zip 根）；
2. 不带 `node_modules/` / `.venv/` / `target/` / `__pycache__/` / `.git/` / `dist/`；
3. 命名 `<id>-<version>-<os>-<arch>.zip`（脚本插件用 `-any.zip`）。

```powershell
# PowerShell：把目录内容打到 zip 根（注意 \* ，不是目录本身）
Compress-Archive -Path .\com.example.weather\* `
                 -DestinationPath .\com.example.weather-1.2.0-windows-x86_64.zip
```

```bash
# bash：进目录再打，同样不套层
cd com.example.weather && zip -r ../com.example.weather-1.2.0-linux-x86_64.zip ./*
```

发 release 时再附一份 `SHA256SUMS`（GNU `sha256sum` 格式）：

```powershell
Get-FileHash .\com.example.weather-1.2.0-windows-x86_64.zip -Algorithm SHA256 |
  ForEach-Object { "$($_.Hash.ToLower())  com.example.weather-1.2.0-windows-x86_64.zip" } |
  Out-File -Encoding ascii SHA256SUMS
```

检查清单：

- [ ] `plugin.json` 在 zip 根，`id` / `version` 与 release tag 一致
- [ ] `entry` 指向的文件真的在 `bin/` 里，名字带不带 `lib` 前缀与平台一致
- [ ] `platforms` 与资产名里的 `<os>-<arch>` 一致
- [ ] 没有 `node_modules` / `.venv` / `target` / 符号链接
- [ ] 附了 `SHA256SUMS`

---

## 4. 兼容期行为

| 形态 | 现在会怎样 | 什么时候不能用了 |
| --- | --- | --- |
| 平铺 `plugins/foo.dll` | ✅ 继续加载（探测仍 `dlopen` 一次） | 兼容期结束（未定日期） |
| 平铺 `foo.dll.disable` | ✅ 继续视为「已禁用」 | 同上 |
| Go 平铺 dll（无 `plugin.json`） | ❌ **直接拒绝**，错误 `GoRequiresManifest`，指路 `plugin migrate` | 永远（这是安全要求，不是兼容期问题） |
| `plugins/scripts/<目录>/` | ✅ 不变，中间插件照常发现 | 脚本插件不在本次迁移范围内 |
| `plugins/<id>/`（bundle） | ✅ 新形态 | — |
| `plugins/.disabled/<id>/` | ✅ 视为已禁用，不加载 | — |
| 目录名 ≠ `id` | ⚠️ 告警，**按 `id` 加载**（不拒绝） | 建议改对，否则 `uninstall <名字>` 会找不到 |
| `plugin_init` 报的名字 ≠ `id` | ❌ **拒绝加载**（`AGENTS.md` 约束 59） | 清单写错了或包装错了，必须改到一致 |

两条容易踩的：

1. **平铺 dll 与 bundle 同时存在 = 同一个插件加载两次**。迁移是「移动」，正常不会两份；
   手工迁移时忘了删平铺那份，症状是「命令回了两次」「服务注册冲突」。
2. **禁用语义变了但并存**：平铺是改名（`.disable`），bundle 是挪目录（`.disabled/`）。
   别把 bundle 目录改名成 `foo.disabled`——那不是禁用，是变成一个「没有 `plugin.json` 的普通目录」，会被跳过且不解释。

---

## 5. 常见错误排查

| 报错 / 现象 | 含义 | 怎么改 |
| --- | --- | --- |
| `BadId`：id 非法 | 含 `/`、`\`、空格，或以 `.` 开头，或超 128 字符 | 用 `[A-Za-z0-9._-]`，以字母数字开头；`plugin_init` 里那个名字也要改（作者侧） |
| `BadEntry`：entry 非法 | 绝对路径、含 `..`、以反斜杠开头 | 写成相对包根的 `bin/xxx.dll` |
| `EntryMissing`：entry 不存在 | 路径写错，或 Linux 上产物带 `lib` 前缀而 `entry` 没写 | `ls plugins/<id>/bin/` 看真名，改 `entry` |
| `EntryEscapes`：entry 跑出包外 | `..` 或符号链接把路径指到包外 | 别用符号链接；`entry` 必须在包内 |
| `UnsupportedPlatform` | `platforms` 不含本机 | 常见是写成 `windows-x64` / `win-x86_64`；必须是 `windows-x86_64` 这种 `<os>-<arch>`（与资产名同一套字面量） |
| `UnsupportedSchema` | `schema` 大于 1 | 改成 `1`；更大的版本要新宿主 |
| `AbiMismatch` | `plugin_init` 返回的 ABI ≠ 宿主 ABI（现在是**精确相等**） | 用对应版本的工具链重编；`plugin.json` 的 `abi` 写对也不能绕过 |
| 目录名 ≠ `id` 的告警 | 目录名与 `plugin.json` 的 `id` 不一致 | 以 `id` 为准加载；把目录改成 `id`，否则 `uninstall` 要用 `id` 找 |
| `plugin_init` 名字 ≠ `id` | 运行时报的名字与清单 id 不一致 | **拒绝加载**，不是告警：清单写错了或包装错了，改到一致（[§4](#4-兼容期行为)） |
| `GoRequiresManifest` | Go 插件没有 `plugin.json` | 补 `plugin.json`（[§3.3](#33-go)）；这是唯一不能靠兼容期绕过的 |
| TUI 商店：sha256 不匹配 | 下载到的字节与索引不符 | 拒绝安装；换网络 / 换镜像重试，别手工绕过 |
| TUI 商店：替换原生 dll 报「拒绝访问」 | 实例在跑、dll 正被映射 | `fuyubot stop` 后重试；装新插件、卸脚本插件不受影响 |
| TUI 商店：索引拉不到 | 网络 / 商城挂了 | 显示缓存索引并标「离线」；已装插件照常可用 |
| `跳过插件 <路径>：<原因>` | 探测失败，只跳过这一个 | 看原因；`--log-level debug` 拿更细的日志 |
| 插件加载了但没反应 | 依赖没满足（`inject` 的服务没人提供），内核没激活它 | `fuyubot plugin list` 看依赖；确认提供方也在、且名字对 |
| 插件加载了但配置读不到 | `[plugin.<名>]` 的段名与插件自报的名字不一致 | 段名 = `plugin_init` 的名字，点分就嵌套（`fuyubot.manage` → `[plugin.fuyubot.manage]`） |

定位手法：

```powershell
fuyubot plugin list                 # 静态看：id / version / ABI / 路径（新形态不 dlopen）
fuyubot run --log-level debug       # 插件日志打到 debug
fuyubot run --log-plugin com.example.weather   # 只看某个插件的日志
fuyubot run --replay examples\replay\new_message.json   # 不接 TDLib，纯离线验
```

`--replay` 那条特别有用：不需要账号、不连 Telegram，把一批更新灌进管线就能看插件有没有反应。

---

## 6. FAQ

**Q：能热更新吗？**
A：**原生插件不行**（分发 P0）。两个原因叠在一起：Windows 上已映射的 dll 删不掉/改不了名，
宿主也没有运行时重新加载的入口（插件只在启动时扫一次）。装完**重启生效**。
脚本插件是例外——中间插件的 watch 会热加载，但那是 SDK 侧行为，与 bundle 无关。
Go 插件更严格：同一进程里不能「卸载再加载」，更新只能重启。

**Q：迁移会不会丢配置？**
A：不会。迁移只移动 dll / 目录，`fuyubot.toml` 一个字都不改。配置在 `[plugin.<id>]` 下，
段名跟着插件自报的名字走——**如果迁移后插件名没变（正常都不会变），配置照常读到**。
唯一要注意的是：你手工把 `id` 改成了别的名字，那配置段也要跟着改。

**Q：卸载会不会删我的数据？**
A：不会。`plugin uninstall` 只删插件目录（`plugins/<id>/` 或 `plugins/scripts/<id>/`），
不删 `fuyubot.toml` 里的配置段，也不删插件自己写的数据。
反过来说，**它也不会帮你清理**——想彻底清干净得自己删配置段和插件的数据目录。

**Q：zip 安装什么时候有？**
A：**已经有了，在 TUI 商店里**（`fuyubot run --tui` → 商店页）：搜索、详情、下载、sha256 校验、解压、安装一条龙。
CLI 的 `fuyubot plugin install` **永远不吃 zip**，它只接受已经解压好的目录——这是刻意的分工：
网络与解压只在商店层，加载器不知道有 zip 这回事。
不想用商店也可以手动：**下载 zip → 解压到 `plugins/<id>/`（解压工具不限）→ 重启**。

**Q：商店会不会偷偷装东西 / 自动更新？**
A：不会。商店只在你点「安装」时才下载，**不自动更新**（只提示有新版本）；
每个包都校验 sha256，未签名的包会显式标注；只有**替换正在被加载的原生 dll** 时才需要先停实例
（装新插件、卸脚本插件随时可以做，重启后生效）。
它只写 `plugins/<id>/`（以及 `.staging` / `.trash` / `.cache` 三个临时目录），别的地方不碰。

**Q：目录名必须等于 id 吗？**
A：必须（契约要求）。不一致时宿主会告警并**按 `id` 加载**，不会拒绝——但
`plugin uninstall <名字>` 是按目录名/id 顺序找的，目录名不对会让卸载变麻烦。
一句话：改对，别依赖告警。

**Q：能不能同时留新旧两份？**
A：不能。平铺 dll 与 bundle 同时存在 = 同一个插件被加载两次，
症状是命令回两次、服务注册冲突、日志重复。迁移是「移动」，正常情况下不会出现；
手工迁移时记得删平铺那份。

**Q：脚本插件也要迁到 `plugins/<id>/` 吗？**
A：分发 P0 不需要，也不会自动迁。脚本插件的家仍是 `plugins/scripts/<目录>/`，
只是可以加一个可选的 `plugin.json` 让 `plugin list` 展示得更清楚。
目录统一是以后的事（[`docs/plugin-store-design.md`](plugin-store-design.md#132-目录统一时机)）。

**Q：我在 APK / 别的宿主里也用这套格式吗？**
A：可以。bundle 格式是**给任何安装器消费的契约**，与宿主是不是本仓无关；
那些宿主自己的商店怎么做，本仓不管，只要求它们产出的目录符合
[目录布局](plugin-store-design.md#2-目录布局分发-p0) 与 [`plugin.json`](plugin-store-design.md#3-pluginjson静态元数据)。

---

## 7. P0 / P1 边界

| 能力 | 阶段 | 在哪 |
| --- | --- | --- |
| `plugins/<id>/` + `plugin.json` + `bin/` 目录格式 | **P0** | 本仓（已落地，2026-11-03） |
| 静态 `probe`（不 `dlopen`）、Go 无 sidecar 拒绝、Go 最后加载 | **P0** | 本仓 |
| `plugin list` / `install <目录>` / `uninstall` / `migrate` | **P0** | 本仓 |
| 平铺 dll 兼容 | **P0** | 本仓（兼容期） |
| **TUI 商店**：拉索引、搜索、详情、下载、sha256、解压、安装 / 卸载 | **P0** | 本仓 `crates/fuyubot-cli/src/panel/store.rs` |
| 插件索引托管 / 商城站点 / 收录审核 | **P1** | **不在本仓**（独立商城仓） |
| 锁文件 `plugins.lock` | **P1（可选）** | **不在本仓** |
| 签名（`SHA256SUMS.minisig` / 索引签名） | **P1（可选）** | **不在本仓** |
| CLI 的 `plugin install <zip>` | **不存在** | 本仓不提供：zip 只在 TUI 商店（或用户手动解压）消费 |

**记住这一条就不会错**：**加载器只吃目录，从不吃 zip**；zip 的唯一入口是 TUI 商店（或手动解压）。

---

## 相关文档

- [`docs/plugin-format-v1.md`](plugin-format-v1.md)：分发 P0 的权威契约（`plugin.json` 字段与校验规则）
- [`docs/plugin-store-design.md`](plugin-store-design.md)：目录布局、加载语义、发布与打包规范、商城契约
- [`docs/design-script-plugin-v2.md`](design-script-plugin-v2.md)：脚本插件为什么还是「一脚本一进程」
