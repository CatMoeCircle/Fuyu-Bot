'use client';

import { useEffect, useState } from 'react';
import { cn } from '@/lib/cn';

/** 主题色：值对应 app/theme-colors.css 里的 `[data-color='…']`，neutral = 默认不覆盖。 */
const COLORS = [
  { value: 'blue', label: '蓝', dot: 'bg-blue-500' },
  { value: 'violet', label: '紫', dot: 'bg-violet-500' },
  { value: 'emerald', label: '绿', dot: 'bg-emerald-500' },
  { value: 'ruby', label: '红', dot: 'bg-rose-500' },
  { value: 'amber', label: '琥珀', dot: 'bg-amber-500' },
  { value: 'neutral', label: '灰', dot: 'bg-zinc-400' },
] as const;

/** 与 app/layout.tsx 里 `<html data-color>` 的默认值保持一致。 */
const DEFAULT_COLOR = 'blue';
const STORAGE_KEY = 'fuyubot-color';

export function ThemeColorPicker() {
  // 初值取服务端写死的默认色，避免首屏闪烁成灰色
  const [color, setColor] = useState<string>(DEFAULT_COLOR);

  useEffect(() => {
    // `?color=violet` 可以分享某个配色；否则读上次的选择，再否则用默认。
    const fromUrl = new URLSearchParams(location.search).get('color');
    const saved = fromUrl ?? localStorage.getItem(STORAGE_KEY) ?? document.documentElement.dataset.color;
    if (saved && COLORS.some((c) => c.value === saved)) {
      setColor(saved);
      if (saved === 'neutral') delete document.documentElement.dataset.color;
      else document.documentElement.dataset.color = saved;
    }
  }, []);

  function pick(value: string) {
    setColor(value);
    localStorage.setItem(STORAGE_KEY, value);
    if (value === 'neutral') delete document.documentElement.dataset.color;
    else document.documentElement.dataset.color = value;
  }

  return (
    <div
      className="fixed end-3 bottom-3 z-50 flex items-center gap-1.5 rounded-full border bg-fd-background/85 p-1.5 shadow-sm backdrop-blur-md"
      role="group"
      aria-label="主题色"
    >
      {COLORS.map((item) => (
        <button
          key={item.value}
          type="button"
          title={item.label}
          aria-label={`主题色：${item.label}`}
          aria-pressed={color === item.value}
          onClick={() => pick(item.value)}
          className={cn(
            'size-5 rounded-full transition-transform hover:scale-110',
            item.dot,
            color === item.value && 'ring-2 ring-fd-ring ring-offset-2 ring-offset-fd-background',
          )}
        />
      ))}
    </div>
  );
}
