// The official extensions for /extensions/: the latest published registry
// from crc-extensions, used only when its signature verifies against the
// same key crc has built in (../src/ext/registry-key.pem). Anything else,
// an unpublished registry, no network, a bad signature, keeps the
// committed src/data/extensions.json and says why. Runs under bun with
// nothing but the standard library.
import { createPublicKey, verify } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, "..", "src", "data", "extensions.json");
const base = "https://github.com/caioricciuti/crc-extensions/releases/latest/download/";

async function fetchBytes(name) {
  const response = await fetch(base + name, { redirect: "follow" });
  if (!response.ok) throw new Error(`${name}: HTTP ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

try {
  const pem = (await readFile(join(here, "..", "..", "src", "ext", "registry-key.pem"), "utf8"))
    .split("\n")
    .filter((line) => !line.startsWith("#"))
    .join("\n");
  const key = createPublicKey(pem);
  const [index, signature] = await Promise.all([fetchBytes("index.json"), fetchBytes("index.json.sig")]);
  if (!verify("sha256", index, key, signature)) throw new Error("the signature does not verify");
  const registry = JSON.parse(index.toString("utf8"));
  const keep = ["id", "name", "version", "description", "authors", "license", "icon", "homepage", "capabilities", "commands", "readme"];
  const extensions = registry.extensions.map((e) => Object.fromEntries(keep.filter((k) => k in e).map((k) => [k, e[k]])));
  await writeFile(out, JSON.stringify({ source: "registry", extensions }, null, 2) + "\n");
  console.log(`extensions: ${extensions.length} from the signed registry`);
} catch (error) {
  console.log(`extensions: keeping the committed list (${error.message})`);
}
