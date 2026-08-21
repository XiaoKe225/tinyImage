/**
 * Pack-only: obfuscate Vite JS/CSS bundles under dist/.
 * Dev / `npm run build` never calls this. Raises reverse-cost; not unbreakable.
 */
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import JavaScriptObfuscator from "javascript-obfuscator";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const distDir = path.join(root, "dist");

if (!fs.existsSync(distDir)) {
  console.error("obfuscate-dist: dist/ missing — run vite build first");
  process.exit(1);
}

const opts = {
  compact: true,
  controlFlowFlattening: true,
  controlFlowFlatteningThreshold: 0.4,
  deadCodeInjection: false,
  debugProtection: false,
  disableConsoleOutput: false,
  identifierNamesGenerator: "hexadecimal",
  renameGlobals: false,
  selfDefending: false,
  stringArray: true,
  stringArrayCallsTransform: true,
  stringArrayEncoding: ["base64"],
  stringArrayThreshold: 0.75,
  transformObjectKeys: false,
  unicodeEscapeSequence: false,
  // Keep Tauri / DOM bridges readable enough to not break IPC
  reservedNames: [
    "^__TAURI",
    "^__TAURI_INTERNALS__",
    "^webkit",
    "^chrome",
  ],
  reservedStrings: [
    "__TAURI__",
    "__TAURI_INTERNALS__",
    "invoke",
    "listen",
    "getCurrentWindow",
  ],
};

function walk(dir, out = []) {
  for (const name of fs.readdirSync(dir)) {
    const p = path.join(dir, name);
    const st = fs.statSync(p);
    if (st.isDirectory()) walk(p, out);
    else if (name.endsWith(".js")) out.push(p);
  }
  return out;
}

const files = walk(distDir);
if (!files.length) {
  console.error("obfuscate-dist: no .js under dist/");
  process.exit(1);
}

let marker = "";
for (const file of files) {
  const src = fs.readFileSync(file, "utf8");
  const result = JavaScriptObfuscator.obfuscate(src, opts);
  const code = result.getObfuscatedCode();
  fs.writeFileSync(file, code, "utf8");
  marker += `${path.relative(distDir, file)}\n`;
  console.log(`obfuscated: ${path.relative(root, file)} (${src.length} -> ${code.length} bytes)`);
}

fs.writeFileSync(path.join(distDir, ".obfuscated"), marker, "utf8");
console.log("obfuscate-dist: OK");
