// diagrams/*.mmd を正本として、README.md と docs/*.html 内の埋め込み図を同期する。
//
// 使い方:
//   node tools/sync-diagrams.mjs          # 埋め込み図を正本の内容で上書き
//   node tools/sync-diagrams.mjs --check  # 同期状態を検査（乖離があれば exit 1）
//
// 対象ブロックは直前に `<!-- diagram: <name> -->` マーカーを持つもののみ。
// <name> は diagrams/<name>.mmd に対応する。新規図は .mmd を作り、
// 挿入したい位置にマーカー＋空ブロックを置いて本スクリプトを実行する。
import { readdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const check = process.argv.includes('--check');

const used = new Set();
let drift = 0;
let errors = 0;

// .mmd は先頭に frontmatter の title を必須とする（---\ntitle: <名>\n---）
const loadDiagrams = () => {
  const dir = join(root, 'diagrams');
  const map = new Map();
  for (const f of readdirSync(dir)) {
    if (!f.endsWith('.mmd')) continue;
    const body = readFileSync(join(dir, f), 'utf8').replace(/\s+$/, '');
    const fm = /^---\r?\n([\s\S]*?)\r?\n---\r?\n/.exec(body);
    if (!fm || !/^title:[ \t]*\S/m.test(fm[1])) {
      errors++;
      console.error(`ERROR diagrams/${f}: frontmatter title が必須（--- / title: <名> / ---）`);
    }
    map.set(f.slice(0, -4), body);
  }
  return map;
};

const eolOf = s => (s.includes('\r\n') ? '\r\n' : '\n');
const toEol = (s, eol) => s.replace(/\r?\n/g, eol);
const escHtml = s => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

// 各ターゲット: { path, pattern, render }
// pattern は (マーカー行+ブロック開始行)(ブロック本文)(ブロック終端) を捕捉する。
const mdTarget = (path) => ({
  path,
  pattern: /(<!-- diagram: ([\w.-]+) -->\r?\n```mermaid\r?\n)[\s\S]*?(```)/g,
  render: (body, eol) => toEol(body, eol) + eol,
});
const targets = [
  mdTarget('README.md'),
  // マニュアル正本（diagrams/docs/*.md）内の埋め込み図も同じ fence 書式で同期
  ...(existsSync(join(root, 'diagrams', 'docs'))
    ? readdirSync(join(root, 'diagrams', 'docs')) : [])
    .filter((f) => f.endsWith('.md'))
    .map((f) => mdTarget(join('diagrams', 'docs', f))),
  ...readdirSync(join(root, 'docs'))
    .filter(f => f.endsWith('.html'))
    .map(f => ({
      path: join('docs', f),
      pattern: /(<!-- diagram: ([\w.-]+) -->\r?\n<pre class="mermaid">\r?\n)[\s\S]*?(<\/pre>)/g,
      render: (body, eol) => toEol(escHtml(body), eol) + eol,
    })),
];

const diagrams = loadDiagrams();

for (const t of targets) {
  const abs = join(root, t.path);
  const src = readFileSync(abs, 'utf8');
  const eol = eolOf(src);
  const next = src.replace(t.pattern, (m, head, name, tail) => {
    const body = diagrams.get(name);
    if (body === undefined) {
      errors++;
      console.error(`ERROR ${t.path}: diagrams/${name}.mmd が存在しない`);
      return m;
    }
    used.add(name);
    return head + t.render(body, eol) + tail;
  });
  if (next === src) continue;
  drift++;
  if (check) {
    console.error(`DRIFT: ${t.path}`);
  } else {
    writeFileSync(abs, next);
    console.log(`synced: ${t.path}`);
  }
}

for (const name of diagrams.keys()) {
  if (!used.has(name)) console.error(`WARN: diagrams/${name}.mmd を参照するマーカーが無い`);
}

if (check && drift > 0) {
  console.error(`${drift} ファイルが diagrams/ と乖離。node tools/sync-diagrams.mjs を実行すること`);
  process.exit(1);
}
if (errors > 0) process.exit(1);
console.log(check ? 'all diagrams in sync' : 'done');
