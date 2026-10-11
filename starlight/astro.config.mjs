import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// `site` and `base` come from the environment so the source stays host-agnostic; CI supplies the
// real Pages URL and subpath. Locally they fall back to a root deployment.
const site = process.env.DOCS_SITE || 'http://localhost:4321';
const base = process.env.DOCS_BASE || '/';

export default defineConfig({
  site,
  base,
  integrations: [
    starlight({
      title: 'loadlab',
      description: 'A programmable Linux workload laboratory',
      sidebar: [
        { slug: 'introduction' },
        {
          label: 'Start here',
          items: [{ slug: 'getting-started' }, { slug: 'concepts' }, { slug: 'examples' }],
        },
        { slug: 'expressions' },
        {
          label: 'Controllers',
          items: [
            { slug: 'controllers/cpu' },
            { slug: 'controllers/memory' },
            { slug: 'controllers/page-access' },
            { slug: 'controllers/file-backed' },
            { slug: 'controllers/shared' },
            { slug: 'controllers/io' },
            { slug: 'controllers/network' },
            { slug: 'controllers/memory-ceiling' },
            { slug: 'controllers/thp' },
          ],
        },
        {
          label: 'HTTP API',
          items: [
            { slug: 'http-api/authentication' },
            { slug: 'http-api/health' },
            { slug: 'http-api/state' },
            { slug: 'http-api/workload' },
            { slug: 'http-api/composite-shape' },
            { slug: 'http-api/start-stop' },
            { slug: 'http-api/errors' },
            { slug: 'http-api/metrics' },
          ],
        },
        {
          label: 'Reference',
          items: [{ slug: 'host' }, { slug: 'configuration' }, { slug: 'architecture' }, { slug: 'deploy' }],
        },
      ],
    }),
  ],
});
