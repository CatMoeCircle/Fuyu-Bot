import { source } from '@/lib/source';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { getLayoutTabs } from 'fumadocs-ui/layouts/shared';
import { BookMarked } from 'lucide-react';
import { baseOptions } from '@/lib/layout.shared';

/**
 * 每个大类自己的颜色（不加背景块，只给图标上色）。
 * light / dark 各一档，保证在两种模式下都有足够对比度。
 * 「总览」用站点主题色（跟随右下角色点），其余四个固定，互相区分开。
 */
const CATEGORY_COLORS: Record<string, string> = {
  // 键是页面树里的 folder url（**不带**结尾斜杠）
  '/docs/usage': 'text-blue-600 dark:text-blue-400',
  '/docs/plugin': 'text-violet-600 dark:text-violet-400',
  '/docs/dev': 'text-amber-600 dark:text-amber-400',
  '/docs/cli': 'text-emerald-600 dark:text-emerald-400',
};
const DEFAULT_COLOR = 'text-fd-primary';

/**
 * 图标容器：`size-full` 填满 fumadocs 下拉的容器（窄屏 `size-9` / md 以上 `size-5`），
 * 内部 svg 固定 16px 并居中——不然窄屏会被挤到左上角。
 */
function CategoryIcon({ children, color }: { children: React.ReactNode; color: string }) {
  return (
    <span className={`inline-flex size-full items-center justify-center [&_svg]:size-4 ${color}`}>
      {children}
    </span>
  );
}

// 四个分类在各自 meta.json 里是 root folder。transform 传恒等函数是为了**跳过** getLayoutTabs
// 默认那层 `size-full` 的 icon wrapper——那层带着 `max-md:p-1.5`，窄屏会把图标挤成一个小环。
const sectionTabs = getLayoutTabs(source.getPageTree(), {
  transform: (tab) => tab,
}).map((tab) => ({
  url: tab.url,
  title: tab.title,
  description: tab.description,
  icon: <CategoryIcon color={CATEGORY_COLORS[tab.url] ?? DEFAULT_COLOR}>{tab.icon}</CategoryIcon>,
}));

const tabs = [
  {
    url: '/docs/',
    title: '总览',
    description: '框架概览与文档地图',
    icon: (
      <CategoryIcon color={DEFAULT_COLOR}>
        <BookMarked />
      </CategoryIcon>
    ),
    // 精确匹配：否则 `/docs/` 会把所有分类下的页面都算成「总览」选中
    urls: new Set(['/docs']),
  },
  ...sectionTabs,
];

export default function Layout({ children }: LayoutProps<'/docs'>) {
  return (
    <DocsLayout tree={source.getPageTree()} tabs={tabs} tabMode="auto" {...baseOptions()}>
      {children}
    </DocsLayout>
  );
}
