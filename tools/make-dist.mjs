// make-dist.mjs — 配布物の作成（README §13 / diagrams/asset-update.mmd 正本）
//
// 使い方: node tools/make-dist.mjs [--skip-build]
//
// 生成物:
//   dist/kasugai-agent/         zip 用ステージング（exe + config.example.json + panel/ + docs/ + prompts/）
//   dist/kasugai-agent-<ver>.zip 配布 zip（exe 隣接に config.json を置く構成）
//   dist/cdn/                   CDN 公開用ディレクトリ（manifest.json + <ch>/ ファイル群）
//     manifest.json = {minExeVersion, channels:{<ch>:{version, files:{<rel>:sha256}}}}
//     assets::check が {cdnBaseUrl}/manifest.json と {cdnBaseUrl}/<ch>/<rel> を取りに来る
//
// CDN 公開は dist/cdn/ を丸ごと静的ホスティングへ置くだけ。
import { execFileSync, execSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const server = path.join(root, "server");
const dist = path.join(root, "dist");
const stage = path.join(dist, "kasugai-agent");
const cdn = path.join(dist, "cdn");
const CHANNELS = ["panel", "docs", "prompts"];
const skipBuild = process.argv.includes("--skip-build");

const sha256 = (b) => createHash("sha256").update(b).digest("hex");
const cp = (src, dst) => {
  fs.mkdirSync(path.dirname(dst), { recursive: true });
  fs.copyFileSync(src, dst);
};
const walk = (dir, base = dir, out = []) => {
  if (!fs.existsSync(dir)) return out;
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, base, out);
    else if (e.isFile()) out.push(path.relative(base, p).replaceAll("\\", "/"));
  }
  return out;
};

// version = Cargo.toml の package.version
const toml = fs.readFileSync(path.join(server, "Cargo.toml"), "utf8");
const pkgVer = /version\s*=\s*"([^"]+)"/.exec(toml)?.[1] || "0.0.0";
const today = new Date().toISOString().slice(0, 10).replaceAll("-", "");
const assetVer = `${pkgVer}-${today}`;

// ---- 1. release build ----
if (!skipBuild) {
  console.log("[dist] cargo build --release ...");
  execSync("cargo build --release", { cwd: server, stdio: "inherit" });
}
const exe = path.join(server, "target", "release", "kasugai-agent.exe");
if (!fs.existsSync(exe)) {
  console.error(`[dist] ${exe} がありません（cargo build --release を先に）`);
  process.exit(1);
}

// ---- 2. zip ステージング ----
fs.rmSync(stage, { recursive: true, force: true });
fs.mkdirSync(stage, { recursive: true });
cp(exe, path.join(stage, "kasugai-agent.exe"));
cp(path.join(root, "config.example.json"), path.join(stage, "config.example.json"));
for (const ch of CHANNELS) {
  for (const rel of walk(path.join(root, ch))) {
    cp(path.join(root, ch, rel), path.join(stage, ch, rel));
  }
}

// ---- 3. zip 化（PowerShell Compress-Archive）----
fs.mkdirSync(dist, { recursive: true });
const zip = path.join(dist, `kasugai-agent-${pkgVer}.zip`);
fs.rmSync(zip, { force: true });
execFileSync("powershell", [
  "-NoProfile", "-Command",
  `Compress-Archive -Path '${stage.replaceAll("'", "''")}\\*' -DestinationPath '${zip.replaceAll("'", "''")}'`,
]);
console.log(`[dist] ${path.relative(root, zip)}`);

// ---- 4. CDN 用 manifest + チャネルファイル ----
fs.rmSync(cdn, { recursive: true, force: true });
fs.mkdirSync(cdn, { recursive: true });
const channels = {};
for (const ch of CHANNELS) {
  const files = {};
  for (const rel of walk(path.join(root, ch))) {
    const data = fs.readFileSync(path.join(root, ch, rel));
    files[rel] = sha256(data);
    cp(path.join(root, ch, rel), path.join(cdn, ch, rel));
  }
  if (Object.keys(files).length) channels[ch] = { version: assetVer, files };
}
fs.writeFileSync(
  path.join(cdn, "manifest.json"),
  JSON.stringify({ generated: new Date().toISOString(), minExeVersion: pkgVer, channels }, null, 2) + "\n",
);
console.log(`[dist] ${path.relative(root, cdn)}/ （manifest + ${Object.keys(channels).join(", ")} v${assetVer}）`);
console.log(`[dist] manifest sha256: ${sha256(fs.readFileSync(path.join(cdn, "manifest.json")))}`);
console.log("[dist] 公開は dist/cdn/ を静的ホスティングへ。cdnManifestSha256 ピンに上記ハッシュを使えます");
