// Mirrors the repository's Markdown into the site's content collection.
//
// The project documents (../CHANGELOG.md, ../CONTRIBUTING.md and a few
// files in ../docs) stay where they are,
// written as plain Markdown for GitHub; this script copies them into
// src/content/docs with the frontmatter Starlight needs and links rewritten
// to site paths. The copies are generated and gitignored. Runs under bun
// with nothing but the standard library.
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const out = join(here, "..", "src", "content", "docs");
const github = "https://github.com/caioricciuti/crc/blob/main/";

// source path (from the repository root) -> site slug, description, and a
// title where the file's own H1 is not the right page name
const pages = [
  ["CHANGELOG.md", "changelog", "One entry per release."],
  ["docs/vision.md", "vision", "The three bets behind the editor."],
  ["docs/extensions.md", "extensions", "How extensions will work: WebAssembly, declared capabilities, signing.", "Extensions: design"],
  ["docs/dependency-review.md", "dependency-review", "Every crate and build script, read."],
  ["CONTRIBUTING.md", "contributing", "How to propose a change."],
];

// The README is not mirrored: the hand-written guide under /docs/ replaces
// it on the site, so links to it land there.
const slugFor = new Map([...pages.map(([path, slug]) => [path, slug]), ["README.md", "docs"]]);

function rewriteLinks(markdown, fromPath) {
  const fromDir = fromPath.includes("/") ? dirname(fromPath) : "";
  return markdown.replace(/\]\(([^)\s]+)\)/g, (whole, target) => {
    if (/^[a-z]+:/.test(target) || target.startsWith("#")) return whole;
    const [file, anchor] = target.split("#");
    // Resolve relative to the file it was written in.
    const parts = (fromDir ? fromDir + "/" : "") + file;
    const normal = parts
      .split("/")
      .reduce((acc, seg) => {
        if (seg === "..") acc.pop();
        else if (seg !== "." && seg !== "") acc.push(seg);
        return acc;
      }, [])
      .join("/");
    // The README's limits section has its own page on the site.
    if (normal === "README.md" && anchor === "what-does-not") return "](/docs/reference/limitations/)";
    const slug = slugFor.get(normal);
    const suffix = anchor ? `#${anchor}` : "";
    if (slug) return `](/${slug}/${suffix})`;
    return `](${github}${normal}${suffix})`;
  });
}

function split(markdown) {
  // The first H1 becomes the page title; Starlight renders its own.
  const match = markdown.match(/^# (.+)\n/m);
  const title = match ? match[1].trim() : "crc";
  const body = match ? markdown.replace(match[0], "") : markdown;
  return { title, body: body.replace(/^\n+/, "") };
}

await mkdir(out, { recursive: true });
for (const [path, slug, description, override] of pages) {
  const source = await readFile(join(repo, path), "utf8");
  const { title, body } = split(source);
  const page = [
    "---",
    `title: ${JSON.stringify(override ?? title)}`,
    `description: ${JSON.stringify(description)}`,
    "---",
    "",
    rewriteLinks(body, path),
  ].join("\n");
  await writeFile(join(out, `${slug}.md`), page);
  console.log(`${path} -> ${slug}.md`);
}
