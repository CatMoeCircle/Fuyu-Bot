import { RootProvider } from 'fumadocs-ui/provider/next';
import './global.css';
import type { Metadata } from 'next';
import { ThemeColorPicker } from '@/components/theme-color-picker';

export const metadata: Metadata = {
  metadataBase: new URL(process.env.NEXT_PUBLIC_SITE_URL ?? 'http://localhost:3000'),
  title: {
    default: 'fuyubot 文档',
    template: '%s · fuyubot',
  },
  description: '基于 TDLib 的 Telegram bot / user 框架：Rust 内核 + C ABI 插件 + 多语言 SDK。',
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html lang="zh-CN" data-color="blue" suppressHydrationWarning>
      <body className="flex flex-col min-h-screen antialiased">
        <RootProvider
          search={{
            // 静态部署：检索在浏览器里做，索引是构建期写死的静态 JSON。
            options: { type: 'static' },
          }}
        >
          {children}
          <ThemeColorPicker />
        </RootProvider>
      </body>
    </html>
  );
}
