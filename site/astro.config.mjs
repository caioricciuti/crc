// crceditor.com. The pages come from the Markdown in ../docs, ../README.md,
// ../CHANGELOG.md and ../CONTRIBUTING.md, mirrored into src/content/docs by
// scripts/sync-docs.mjs before every build. Edit those files, not the copies.
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

export default defineConfig({
  site: "https://crceditor.com",
  integrations: [
    starlight({
      title: "crc",
      description:
        "A text editor for macOS, drawn on the GPU, with a terminal, local Git and Claude Code built in.",
      social: [
        { icon: "github", label: "GitHub", href: "https://github.com/caioricciuti/crc" },
      ],
      customCss: ["./src/styles/custom.css"],
      sidebar: [
        {
          label: "Start",
          items: [
            { label: "Handbook", slug: "handbook" },
            { label: "Changelog", slug: "changelog" },
          ],
        },
        {
          label: "Project",
          items: [
            { label: "Why crc exists", slug: "vision" },
            { label: "Dependency review", slug: "dependency-review" },
            { label: "Contributing", slug: "contributing" },
          ],
        },
      ],
      lastUpdated: false,
      pagination: false,
    }),
  ],
});
