import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // 静态部署：`next build` 直接把整站写进 `out/`，不需要 Node 运行时。
  // 代价是不能用 middleware / 动态 route handler —— 所以 proxy.ts 已删，
  // 搜索改用 `staticGET`（构建期把 orama 索引写成静态 JSON）。
  output: 'export',
  trailingSlash: true,
  images: {
    unoptimized: true,
  },
};

export default withMDX(config);
