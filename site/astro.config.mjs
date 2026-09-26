// crceditor.com. The landing page is src/pages/index.astro; everything under
// /docs/ is hand-written MDX in src/content/docs/docs. The project pages
// (changelog, vision, extensions, dependency review, contributing) come from the
// repository's Markdown, mirrored into src/content/docs by
// scripts/sync-docs.mjs before every build. Edit those files, not the copies.
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

export default defineConfig({
  site: "https://crceditor.com",
  redirects: {
    "/handbook": "/docs/",
  },
  integrations: [
    starlight({
      title: "crc",
      description:
        "A text editor for macOS, drawn on the GPU, with a terminal, local Git and Claude Code built in.",
      logo: { src: "./src/assets/logo.svg", replacesTitle: false },
      favicon: "/favicon.svg",
      social: [
        { icon: "github", label: "GitHub", href: "https://github.com/caioricciuti/crc" },
      ],
      components: {
        SocialIcons: "./src/components/docs/HeaderLinks.astro",
      },
      customCss: ["./src/styles/custom.css"],
      expressiveCode: {
        themes: ["github-dark-dimmed", "github-light"],
        styleOverrides: {
          borderRadius: "10px",
          borderColor: "var(--crc-frame)",
          codeFontFamily: "var(--sl-font-mono)",
          uiFontFamily: "var(--sl-font)",
          frames: {
            shadowColor: "transparent",
          },
        },
      },
      sidebar: [
        {
          label: "Start here",
          items: [
            { label: "Overview", slug: "docs" },
            { label: "Installation", slug: "docs/getting-started/installation" },
            { label: "A tour of the window", slug: "docs/getting-started/tour" },
            { label: "Settings", slug: "docs/getting-started/settings" },
          ],
        },
        {
          label: "Editing",
          items: [
            { label: "Editing text", slug: "docs/features/editing" },
            { label: "Tabs and split panes", slug: "docs/features/tabs-and-panes" },
            { label: "Search and navigation", slug: "docs/features/search-and-navigation" },
            { label: "Syntax highlighting", slug: "docs/features/syntax-highlighting" },
            { label: "Completion", slug: "docs/features/completion" },
            { label: "Language servers", slug: "docs/features/language-servers" },
          ],
        },
        {
          label: "Built in",
          items: [
            { label: "Claude Code", slug: "docs/features/claude-code", badge: { text: "New", variant: "success" } },
            { label: "Terminal", slug: "docs/features/terminal" },
            { label: "Git", slug: "docs/features/git" },
            { label: "HTTP requests", slug: "docs/features/http-requests" },
            { label: "Markdown and previews", slug: "docs/features/markdown-and-previews" },
            { label: "Files and saving", slug: "docs/features/files" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Keyboard shortcuts", slug: "docs/reference/keyboard-shortcuts" },
            { label: "Performance", slug: "docs/reference/performance" },
            { label: "Known limits", slug: "docs/reference/limitations" },
            { label: "Where crc keeps things", slug: "docs/reference/storage" },
            { label: "Architecture", slug: "docs/reference/architecture" },
            { label: "Reporting a problem", slug: "docs/reference/reporting-problems" },
          ],
        },
        {
          label: "Project",
          items: [
            { label: "Changelog", slug: "changelog" },
            { label: "Why crc exists", slug: "vision" },
            { label: "Extensions design", slug: "extensions" },
            { label: "Dependency review", slug: "dependency-review" },
            { label: "Contributing", slug: "contributing" },
          ],
        },
      ],
      lastUpdated: false,
      pagination: true,
    }),
  ],
});
