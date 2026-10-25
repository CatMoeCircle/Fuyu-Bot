import { source } from '@/lib/source';
import { createFromSource } from 'fumadocs-core/search/server';

// 静态部署下没有服务端：构建期把这个索引写成静态 JSON，浏览器端做检索。
export const dynamic = 'force-static';
export const { staticGET: GET } = createFromSource(source);
