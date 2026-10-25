import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { appName, gitConfig } from './shared';

// 四个分类不在这里列：它们由 docs 布局的 root folder 渲染成侧栏顶部的下拉（见 app/docs/layout.tsx）。
// 这里只放首页也会用到的品牌、导航与 GitHub 入口。
export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <span className="inline-flex items-center gap-2 font-semibold">
          <span
            className="size-6 rounded-full shadow-inner"
            style={{
              backgroundImage:
                'linear-gradient(135deg, var(--color-fd-primary), var(--color-fd-ring))',
            }}
          />
          {appName}
        </span>
      ),
    },
    links: [{ text: '文档', url: '/docs/' }],
    githubUrl: `https://github.com/${gitConfig.user}/${gitConfig.repo}`,
  };
}
