#!/usr/bin/env node
// hunkfilter.mjs — git diff -U0 の hunk をインデックス指定で選択し、
// 選択 hunk だけの patch を stdout へ出す（git apply --cached 用）。
// 使い方:
//   node tools/hunkfilter.mjs <file>            — hunk 一覧（index+先頭行）
//   node tools/hunkfilter.mjs <file> <i,j,k...> — 選択 hunk の patch を出力
import { execSync } from 'node:child_process';

const file = process.argv[2];
const pick = (process.argv[3] || '').split(',').map((s) => parseInt(s, 10)).filter((n) => !isNaN(n));
const diff = execSync(`git diff -U0 -- "${file}"`, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });

// ヘッダ（diff --git 〜 最初の @@）と hunk 群に分割
const lines = diff.split('\n');
let hi = lines.findIndex((l) => l.startsWith('@@'));
const header = lines.slice(0, hi).join('\n');
const hunks = [];
let cur = null;
for (const l of lines.slice(hi)) {
  if (l.startsWith('@@')) {
    if (cur) hunks.push(cur);
    cur = [l];
  } else if (cur) {
    cur.push(l);
  }
}
if (cur) hunks.push(cur);

if (!process.argv[3]) {
  hunks.forEach((h, i) => {
    const adds = h.filter((l) => l.startsWith('+')).slice(1);
    const dels = h.filter((l) => l.startsWith('-'));
    console.log(`--- hunk ${i}: ${h[0]}`);
    dels.slice(0, 4).forEach((l) => console.log('  ' + l));
    adds.slice(0, 8).forEach((l) => console.log('  ' + l));
    if (adds.length > 8) console.log(`  ... (+${adds.length - 8} more)`);
  });
} else {
  const out = [header];
  for (const i of pick) if (hunks[i]) out.push(hunks[i].join('\n'));
  process.stdout.write(out.join(''));
}
