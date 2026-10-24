// Enforces the bundle budget after `vite build`; the build fails if either grows past it.
// Gzip ceilings: 50 KB JS includes native routing diagnostics and the live overage
// control; CSS retains the original 6,853-byte ceiling.
import { readdirSync, readFileSync, copyFileSync } from "node:fs";
import { gzipSync } from "node:zlib";

const BUDGET = { js: 50_000, css: 6_853 };

copyFileSync("dist/index.html", "dist/management.html");
const html = readFileSync("dist/index.html", "utf8");
const assets = readdirSync("dist/assets");
const gz = (text) => gzipSync(text).length;
const inline = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]).join("\n");
const size = (ext) =>
  assets.filter((f) => f.endsWith(ext)).reduce((n, f) => n + gz(readFileSync(`dist/assets/${f}`)), 0);
const total = { js: size(".js") + (inline ? gz(inline) : 0), css: size(".css") };

let failed = false;
for (const kind of ["js", "css"]) {
  const over = total[kind] > BUDGET[kind];
  failed ||= over;
  console.log(
    `${kind.toUpperCase().padEnd(3)} ${String(total[kind]).padStart(6)} B gzip  budget ${BUDGET[kind]} B  ${over ? "OVER" : `${BUDGET[kind] - total[kind]} B spare`}`,
  );
}
console.log(`HTML ${gz(html)} B gzip (icon sprite and theme script)`);
if (failed) {
  console.error("Bundle budget exceeded.");
  process.exitCode = 1;
}
