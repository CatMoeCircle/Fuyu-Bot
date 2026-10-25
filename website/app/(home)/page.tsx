import Link from 'next/link';

export default function HomePage() {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-8 px-6 py-24 text-center">
      <div className="space-y-4">
        <h1 className="text-4xl font-bold tracking-tight">fuyubot</h1>
        <p className="max-w-xl text-fd-muted-foreground">
          基于 TDLib 的 Telegram bot / user 框架：Rust 内核 + C ABI 插件 + 多语言 SDK。
          插件就是动态库，Rust / C / C++ / Python / JS / TypeScript 都能写。
        </p>
      </div>

      <div className="flex flex-wrap items-center justify-center gap-3">
        <Link
          href="/docs/usage"
          className="rounded-full bg-fd-primary px-6 py-2.5 font-medium text-fd-primary-foreground"
        >
          使用文档
        </Link>
        <Link
          href="/docs/plugin"
          className="rounded-full border px-6 py-2.5 font-medium"
        >
          插件开发
        </Link>
      </div>

      <p className="max-w-lg text-sm text-fd-muted-foreground">
        不接账号也能验证插件：<code className="font-mono">fuyubot run --replay</code>{' '}
        把一批更新 JSON 灌进整条管线。
      </p>
    </div>
  );
}
