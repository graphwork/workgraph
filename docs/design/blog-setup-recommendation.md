# Blog Setup Recommendation: graphwork.github.io

**Task:** research-blog-setup  
**Date:** 2026-03-17  
**Status:** Research complete

---

## 1. Current State

### graphwork.github.io Repository
- **Repo:** `graphwork/graphwork.github.io` (exists, public)
- **GitHub Pages:** Configured, built from `main` branch root (`/`)
- **Current content:** Static landing page with:
  - `index.html` — hand-written product landing page
  - `style.css` — CSS with JetBrains Mono font, light/dark mode via `prefers-color-scheme`
  - `workgraph-manual.html/.md/.pdf` — pandoc-generated manual
  - `organizational-patterns.html/.md/.pdf` — pandoc-generated theory doc
  - `favicon.svg`, `og-image.png`, font files (`.woff2`)
- **Build system:** None (raw static files served directly)
- **Deployment:** Legacy GitHub Pages (no Actions workflow)

### Key Constraint
The existing landing page and documentation files must be preserved. The blog must coexist with the current site structure.

---

## 2. SSG Recommendation: **Astro**

### Why Astro (over alternatives)

| Criterion | Astro | Next.js | Hugo |
|-----------|-------|---------|------|
| **GitHub Pages simplicity** | Excellent — first-class static output, official GH Pages guide | Requires `output: 'export'` config, more complex deployment | Excellent |
| **Blog aesthetics / dark mode** | Full control via CSS/Tailwind, component-based | Full control but heavier runtime | Template-based, less flexible |
| **Markdown blog posts** | Native content collections, frontmatter, MDX support | Requires extra config (contentlayer/mdx) | Native, excellent |
| **JS shipped to client** | Zero JS by default (islands architecture) | Ships React runtime (~80KB+) | Zero JS |
| **Learning curve** | Low — HTML-like `.astro` files | Medium — React knowledge required | Medium — Go templating |
| **Tailwind integration** | First-class `@astrojs/tailwind` integration | First-class | Requires extra setup |
| **TOC generation** | Built-in via `rehype-toc` / `rehype-slug` | Manual setup | Built-in |
| **Build speed** | Fast (Vite-based) | Slower | Fastest |
| **Coexistence with existing files** | `public/` folder passthrough — existing files drop in | Same via `public/` | `static/` folder |

### Rationale
1. **Zero client JS** — A blog doesn't need React. Astro ships zero JS by default, making pages fast and lightweight. The reference site uses Next.js, but that's overkill for a static blog.
2. **Content collections** — Astro's content collections provide type-safe frontmatter validation, automatic slug generation, and excellent Markdown/MDX support out of the box.
3. **Tailwind v4** — First-class integration. Perfect for replicating the reference site's utility-class-based design system.
4. **GitHub Pages** — Astro has an official `@astrojs/sitemap` and GitHub Actions adapter. One config line: `site: 'https://graphwork.github.io'`.
5. **Passthrough for existing files** — Everything in `public/` is served as-is. The existing manual, patterns docs, fonts, and favicon can live there unchanged.
6. **Simplicity** — `.astro` files are essentially HTML with scoped styles and frontmatter. No framework to learn.

### Runner-up: Hugo
Hugo would also work well (fast builds, native Markdown, zero JS). However, Go templates are less intuitive than Astro's HTML-like syntax, and Tailwind integration requires more setup. Hugo is the right choice if the team wants to avoid Node.js entirely.

---

## 3. Design Tokens (Matching Reference)

### Color Palette

```css
:root {
  /* Backgrounds */
  --bg-primary: #020103;        /* Deep black — page background */
  --bg-secondary: #0a0a0b;     /* Slightly lighter — card/sidebar backgrounds */
  --bg-tertiary: #18181b;      /* zinc-900 — hover states, input backgrounds */
  --bg-input: #27272a;         /* zinc-800 — form inputs */

  /* Text */
  --text-primary: #e4e4e7;     /* zinc-200 — body text */
  --text-secondary: #a1a1aa;   /* zinc-400 — muted/meta text */
  --text-tertiary: #71717a;    /* zinc-500 — very muted text */

  /* Accent */
  --accent-primary: #8b5cf6;   /* violet-500 — links, highlights */
  --accent-hover: #a78bfa;     /* violet-400 — link hover */

  /* Borders */
  --border-primary: #27272a;   /* zinc-800 — card borders, dividers */
  --border-highlight: #3f3f46; /* zinc-700 — subtle emphasis borders */

  /* Gradient (for decorative dividers) */
  --gradient-divider: linear-gradient(90deg, transparent 0%, var(--border-highlight) 50%, transparent 100%);
}
```

### Typography

```css
:root {
  /* Font families */
  --font-body: 'Inter', system-ui, -apple-system, sans-serif;
  --font-mono: 'JetBrains Mono', ui-monospace, monospace;  /* keep existing */

  /* Font sizes */
  --text-xs: 0.75rem;     /* 12px — labels */
  --text-sm: 0.875rem;    /* 14px — metadata, nav */
  --text-base: 1rem;      /* 16px — body */
  --text-lg: 1.125rem;    /* 18px — prose body (blog content) */
  --text-xl: 1.25rem;     /* 20px */
  --text-2xl: 1.5rem;     /* 24px — h3 */
  --text-3xl: 1.875rem;   /* 30px — h2 */
  --text-4xl: 2.25rem;    /* 36px — h1 mobile */
  --text-5xl: 3rem;       /* 48px — h1 desktop */

  /* Line heights */
  --leading-tight: 1.25;  /* headings */
  --leading-normal: 1.5;  /* UI text */
  --leading-relaxed: 1.75; /* prose body */

  /* Font weights */
  --font-normal: 400;
  --font-medium: 500;
  --font-semibold: 600;
  --font-bold: 700;

  /* Tracking */
  --tracking-widest: 0.1em;  /* uppercase labels */
}
```

### Spacing & Layout

```css
:root {
  /* Container */
  --max-w-content: 80rem;     /* 1280px — outer container (max-w-7xl) */
  --max-w-prose: 65ch;        /* blog prose column */

  /* Spacing scale (Tailwind default) */
  --space-1: 0.25rem;   /* 4px */
  --space-2: 0.5rem;    /* 8px */
  --space-3: 0.75rem;   /* 12px */
  --space-4: 1rem;      /* 16px */
  --space-6: 1.5rem;    /* 24px */
  --space-8: 2rem;      /* 32px */
  --space-12: 3rem;     /* 48px */
  --space-16: 4rem;     /* 64px */
  --space-24: 6rem;     /* 96px — sticky top offset */

  /* Sidebar */
  --sidebar-width: 16rem;     /* 256px — w-64 */
  --sidebar-top: 6rem;        /* sticky offset */

  /* Border radius */
  --radius-sm: 0.25rem;
  --radius-md: 0.375rem;
  --radius-lg: 0.5rem;
  --radius-full: 9999px;
}
```

### Responsive Breakpoints

```css
/* Tailwind v4 defaults */
--breakpoint-sm: 640px;
--breakpoint-md: 768px;
--breakpoint-lg: 1024px;
--breakpoint-xl: 1280px;   /* sidebar appears here */
--breakpoint-2xl: 1536px;
```

---

## 4. File Structure

```
graphwork.github.io/
├── astro.config.mjs          # Astro config (site URL, integrations)
├── tailwind.config.mjs       # Tailwind config (extend theme with tokens)
├── tsconfig.json             # TypeScript config
├── package.json              # Dependencies
├── .github/
│   └── workflows/
│       └── deploy.yml        # GitHub Actions: build & deploy to Pages
├── public/                   # PASSTHROUGH — existing files go here
│   ├── favicon.svg           # (existing)
│   ├── og-image.png          # (existing)
│   ├── JetBrainsMono-Regular.woff2  # (existing)
│   ├── JetBrainsMono-Bold.woff2     # (existing)
│   ├── workgraph-manual.html        # (existing)
│   ├── workgraph-manual.md          # (existing)
│   ├── workgraph-manual.pdf         # (existing)
│   ├── organizational-patterns.html # (existing)
│   ├── organizational-patterns.md   # (existing)
│   └── organizational-patterns.pdf  # (existing)
├── src/
│   ├── layouts/
│   │   ├── BaseLayout.astro   # HTML shell, meta tags, Inter font import
│   │   └── BlogPost.astro     # Blog post layout (content + sidebar TOC)
│   ├── components/
│   │   ├── Header.astro       # Site nav (logo + links)
│   │   ├── Footer.astro       # Footer with gradient divider
│   │   ├── TableOfContents.astro  # Sticky sidebar TOC
│   │   ├── BlogCard.astro     # Post card for listing page
│   │   └── GradientDivider.astro  # Reusable gradient line
│   ├── pages/
│   │   ├── index.astro        # Landing page (port existing index.html)
│   │   └── blog/
│   │       ├── index.astro    # Blog listing page
│   │       └── [...slug].astro # Dynamic blog post pages
│   ├── content/
│   │   ├── config.ts          # Content collection schema
│   │   └── blog/
│   │       └── sample-post.md # First blog post (validates pipeline)
│   └── styles/
│       └── global.css         # Tailwind directives + custom tokens + prose overrides
└── .gitignore
```

### Key Decisions
- **Existing files → `public/`**: All current static files move to `public/` for passthrough serving. URLs remain unchanged.
- **Landing page → `src/pages/index.astro`**: Port the current `index.html` into an Astro component. Same content, new system.
- **Blog at `/blog/`**: Blog listing and posts live under `/blog/` path.
- **Content collections**: Blog posts are Markdown files in `src/content/blog/` with typed frontmatter.

---

## 5. Blog Post Frontmatter Schema

```typescript
// src/content/config.ts
import { defineCollection, z } from 'astro:content';

const blog = defineCollection({
  type: 'content',
  schema: z.object({
    title: z.string(),
    description: z.string(),
    date: z.date(),
    author: z.string().default('Workgraph Team'),
    readTime: z.string().optional(),  // e.g., "8 min read"
    tags: z.array(z.string()).default([]),
    draft: z.boolean().default(false),
    image: z.string().optional(),     // OG image override
  }),
});

export const collections = { blog };
```

---

## 6. GitHub Actions Deployment

```yaml
# .github/workflows/deploy.yml
name: Deploy to GitHub Pages

on:
  push:
    branches: [main]
  workflow_dispatch:

permissions:
  contents: read
  pages: write
  id-token: write

concurrency:
  group: pages
  cancel-in-progress: false

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
      - run: npm ci
      - run: npm run build
      - uses: actions/upload-pages-artifact@v3
        with:
          path: dist/

  deploy:
    needs: build
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - id: deployment
        uses: actions/deploy-pages@v4
```

**Note:** The repo's GitHub Pages source must be changed from "Deploy from branch" to "GitHub Actions" in Settings → Pages after the workflow is added.

---

## 7. Key Dependencies

```json
{
  "dependencies": {
    "astro": "^5.x"
  },
  "devDependencies": {
    "@astrojs/tailwind": "^6.x",
    "@astrojs/sitemap": "^4.x",
    "tailwindcss": "^4.x",
    "rehype-slug": "^6.x",
    "rehype-autolink-headings": "^7.x",
    "@tailwindcss/typography": "^0.5.x",
    "sharp": "^0.33.x"
  }
}
```

---

## 8. Special Styling Details

### Blockquotes
```css
.prose blockquote {
  border-left: 3px solid var(--accent-primary);
  padding-left: var(--space-6);
  color: var(--text-secondary);
  font-style: italic;
}
```

### Gradient Divider
```css
.gradient-divider {
  height: 1px;
  background: linear-gradient(90deg, transparent 0%, var(--border-highlight) 50%, transparent 100%);
  border: none;
}
```

### Sticky Table of Contents
```css
.toc-sidebar {
  position: sticky;
  top: var(--sidebar-top);
  max-height: calc(100vh - var(--sidebar-top));
  overflow-y: auto;
  width: var(--sidebar-width);
}

/* Hidden below xl breakpoint */
@media (max-width: 1279px) {
  .toc-sidebar { display: none; }
}
```

### Code Blocks
```css
.prose pre {
  background: var(--bg-secondary);
  border: 1px solid var(--border-primary);
  border-radius: var(--radius-lg);
  padding: var(--space-4);
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  overflow-x: auto;
}
```

### Blog Post Card (Listing Page)
```css
.blog-card {
  border-bottom: 1px solid var(--border-primary);
  padding: var(--space-8) 0;
}
.blog-card:hover .blog-card-title {
  color: var(--accent-primary);
}
.blog-card-meta {
  font-family: var(--font-mono);
  font-size: var(--text-sm);
  color: var(--text-tertiary);
  text-transform: uppercase;
  letter-spacing: var(--tracking-widest);
}
```

---

## 9. Scope Confirmation

This blog is for the **workgraph project** specifically:
- The graphwork.github.io repo description is "Workgraph - Task coordination for humans and AI agents"
- The existing landing page is the workgraph product page
- Content will be workgraph-related posts (project updates, technical deep dives, design philosophy)

---

## 10. Migration Notes

1. **Switch Pages deploy method** from "Deploy from branch" to "GitHub Actions" in repo settings
2. **Move existing static files** to `public/` directory
3. **Port `index.html`** to `src/pages/index.astro` (preserve all content and meta tags)
4. **Remove old `style.css`** from root once ported to Tailwind/global.css (keep font files in `public/`)
5. **Update `.pandoc-template.html`** if pandoc builds are still needed, or move doc generation to a build step

---

## Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| SSG | **Astro** | Zero JS, native content collections, simple deployment |
| Styling | **Tailwind CSS v4** + `@tailwindcss/typography` | Matches reference site's approach, utility-first |
| Font | **Inter** (body) + **JetBrains Mono** (code/meta) | Inter matches reference; JetBrains Mono already in use |
| Blog path | `/blog/` | Standard, keeps root for landing page |
| Deployment | GitHub Actions → GitHub Pages | Modern, cacheable, supports build step |
| Existing files | Passthrough via `public/` | Zero disruption to existing URLs |
