# fuyubot 文档站

用 [Fumadocs](https://fumadocs.dev)（Next.js + MDX）搭的文档站，**静态导出**（`out/`），不需要 Node 运行时。

```powershell
cd website
pnpm install     # 一次就够
pnpm dev         # 本地写文档：http://localhost:3000
pnpm build       # 静态产物：website/out/
pnpm preview     # 用静态服务器预览 out/（python -m http.server 3211）
```

## 部署

`pnpm build` 之后把 **`out/` 整个目录**丢给任意静态托管即可（GitHub Pages / Netlify / Cloudflare Pages / OSS / nginx）。
没有任何服务端组件：`next start` 已从 scripts 里删掉，它跑不了静态导出。

- **GitHub Pages**：`out/` 里的 `_next/` 以 `_` 开头，Jekyll 会忽略它 —— `public/.nojekyll` 已经放好，
  构建时会一起拷进 `out/`。404 由 `out/404.html` 兜底。
- **绝对 URL**：OG 图 / 站点元数据需要一个正式域名，构建时设 `NEXT_PUBLIC_SITE_URL`（默认 `http://localhost:3000`）。
  `next build` 会把它烘进产物，所以要改就在**构建前**改。

## 内容

在 `content/docs/`，**四个一级分类各是一个 root folder**（各自 `meta.json` 里 `"root": true`），
它们在侧栏顶部渲染成一个**分类下拉**（Fumadocs 16 Layout Tabs 的 `tabMode="auto"`）：

| 目录 | 内容 | 图标 |
| --- | --- | --- |
| `content/docs/usage/` | 使用文档：下载安装、配置、运行、回放、实例管理、排错 | `BookOpen` |
| `content/docs/plugin/` | 插件开发：Rust / C / C++ / Python / JS / TS、事件、TDLib、ABI、调试 | `Puzzle` |
| `content/docs/dev/` | 开发文档：架构与数据流、构建测试、常见陷阱、发布 | `Wrench` |
| `content/docs/cli/` | CLI 文档：`fuyubot` 与 `fuyubot-sdk` 的完整命令参考 | `Terminal` |

- **分类的选择器是侧栏顶部那个下拉**（Fumadocs 的 `SidebarTabsDropdown`，`tabMode="auto"`），
  选项里显示「图标 + 标题 + 一句话说明」，说明来自各 `meta.json` 的 `description`。
  代码在 `app/docs/layout.tsx`：`getLayoutTabs(source.getPageTree())` 从 root folder 自动推导标签。
- 还多一个「总览」标签指向 `/docs/`（它在所有 root folder 之外，用 `urls: new Set(['/docs'])` 精确匹配，
  否则会被当成所有页面的前缀高亮）；这样总览页也有这个下拉，而不是一条空壳。
- 分类图标在 `app/docs/layout.tsx` 里统一包一层定尺寸容器（`size-full` 填满 fumadocs 下拉的
  `size-9` / `size-5`，里面 `[&_svg]:size-4` 居中）。**不加背景块**，而是按大类给图标本身配色：
  `CATEGORY_COLORS` 里使用文档=蓝、插件开发=紫、开发文档=琥珀、CLI 文档=绿（各有 dark 变体），
  总览用站点主题色。`getLayoutTabs(tree, { transform: t => t })` 那个恒等 transform 是**必须**的
  ——默认 transform 会给图标套一层 `max-md:p-1.5` wrapper，窄屏会缩成一个小环。
  落地页 / 移动端抽屉里的侧栏树图标由 `app/global.css` 里 `#nd-sidebar a[href='/docs/<分类>/'] svg`
  四条规则上同样的色。
  原始 lucide 名写在各自 `meta.json` 的 `"icon"`，由 `lib/source.ts` 的 `lucideIconsPlugin()` 解析。
- **主题色**：`app/theme-colors.css` 给 fumadocs 的 `--color-fd-*` 定义了 5 个配色（blue / violet /
  emerald / ruby / amber，各带 `.dark` 变体），挂在 `<html data-color="…">` 上，**默认 `blue`**
  （`app/layout.tsx` 里写死，所以首屏不是灰色）；`components/theme-color-picker.tsx` 是右下角那排色点，
  选择存 `localStorage`，也认 `?color=violet` 这种分享链接，最后一个是 neutral（去掉属性 = 原始灰）。
- 每页 frontmatter 至少要有 `title` 和 `description`。
- 站点信息（名字 / GitHub / 顶部一条「文档」入口）在 `lib/shared.ts` 与 `lib/layout.shared.tsx`
  （品牌是「渐变圆点 + fuyubot」）；**四个分类不写在那里**——它们由 root folder 承担。
- 手册类原始文档仍在仓库根的 [`docs/`](../docs/)，这是面向使用者的站点版本；**两边都要维护**。

## 静态导出带来的两个限制

- **没有 middleware**：scaffold 自带的 `proxy.ts`（按 `Accept` 把 `/docs/x` 改写成 Markdown）已删，
  静态部署下没有请求期改写。Markdown 原文仍然在固定路由：`/llms.mdx/docs/<路径>/content.md`，
  索引 `/llms.txt`、全文 `/llms-full.txt`。
- **搜索是浏览器端检索**：`app/api/search/route.ts` 用 `staticGET`，构建期把 orama 索引写成
  一个静态 JSON（`out/api/search`），前端用 `type: 'static'` 加载它。
