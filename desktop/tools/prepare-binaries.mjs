import { copyFile, mkdir, stat } from "node:fs/promises";
import { resolve } from "node:path";
import { execFileSync } from "node:child_process";

const source = process.argv[2];
if (!source) throw new Error("Provide the directory containing minterd and minterctl");
const target = process.argv[3] ??
  execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (.+)$/m)?.[1];
if (!target) throw new Error("Cannot determine Rust target");
const suffix = target.includes("windows") ? ".exe" : "";
const directory = resolve(import.meta.dirname, "../src-tauri/binaries");
await mkdir(directory, { recursive: true });
for (const name of ["minterd", "minterctl"]) {
  const file = resolve(source, name + suffix);
  if (!(await stat(file)).isFile()) throw new Error(`Missing executable ${file}`);
  await copyFile(file, resolve(directory, `${name}-${target}${suffix}`));
}
console.log(`Prepared Minter binaries for ${target}`);
