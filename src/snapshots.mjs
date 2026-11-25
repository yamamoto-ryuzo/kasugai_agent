// chat が変更したファイルの turn 単位変更前スナップショット採取・復元
//
// chat-snapshots/<chatId>/ 配下にスナップショット本体（sNNNN）と
// manifest.json（エントリ一覧）を置く。エントリは
// { path, turn, snap, existed, ts } — path は正規化済み絶対パス、
// snap は dir 内のファイル名（existed=false の新規作成ファイルは null）。
// turn ごとに「その turn で最初に触った時点」の内容を保持するので、
// 「turn N 開始時点へ戻す」= turn N 以降に触った各ファイルを
// その turn 最古のスナップショットへ書き戻す（当時無ければ削除）で実現する。
// ディスク永続化のため session/load 復元後・control 再起動後でも戻せる。
import { existsSync, mkdirSync, readFileSync, writeFileSync, copyFileSync,
         rmSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';

export class SnapshotStore {
  constructor(dir) {
    this.dir = dir;
    this.m = this.#load();
  }

  #load() {
    try {
      const m = JSON.parse(readFileSync(join(this.dir, 'manifest.json'), 'utf8'));
      if (m && Array.isArray(m.entries)) return { seq: m.seq || 0, prompts: m.prompts || {}, entries: m.entries };
    } catch { /* noop */ }
    return { seq: 0, prompts: {}, entries: [] };
  }

  #save() {
    try {
      mkdirSync(this.dir, { recursive: true });
      writeFileSync(join(this.dir, 'manifest.json'), JSON.stringify(this.m, null, 2));
    } catch { /* noop */ }
  }

  // turn 開始時に呼ぶ。prompt 表示用に先頭を記録する
  beginTurn(turn, text) {
    if (!this.m.prompts[turn]) {
      this.m.prompts[turn] = { text: String(text ?? '').replace(/\s+/g, ' ').slice(0, 60),
        ts: new Date().toISOString() };
      this.#save();
    }
  }

  // path（正規化済み）の現行内容を turn のスナップショットとして採取。
  // 同一 turn 内では最初の1回のみ（最古＝その turn 開始時点の状態）
  capture(path, turn) {
    if (!path || this.m.entries.some((e) => e.path === path && e.turn === turn)) return;
    const existed = existsSync(path);
    let snap = null;
    if (existed) {
      snap = 's' + String(++this.m.seq).padStart(4, '0');
      try {
        mkdirSync(this.dir, { recursive: true });
        copyFileSync(path, join(this.dir, snap));
      } catch { --this.m.seq; return; }
    }
    this.m.entries.push({ path, turn, snap, existed, ts: new Date().toISOString() });
    this.#save();
  }

  // 復元ポイント一覧: [{turn, prompt, files, ts}]（turn 昇順）
  points() {
    const by = new Map();
    for (const e of this.m.entries) {
      const p = by.get(e.turn) ?? { turn: e.turn, prompt: this.m.prompts[e.turn]?.text ?? '', files: [], ts: e.ts };
      if (!p.files.includes(e.path)) p.files.push(e.path);
      by.set(e.turn, p);
    }
    return [...by.values()].sort((a, b) => a.turn - b.turn);
  }

  lastTurn() {
    return this.m.entries.reduce((t, e) => Math.max(t, e.turn),
      Object.keys(this.m.prompts).reduce((t, k) => Math.max(t, +k), 0));
  }

  // turn N 以降の変更を元に戻す。turn 省略時は最新ポイントのみ戻す。
  // 各ファイルは turn>=N の最古エントリの内容へ戻し、当時無ければ削除する。
  // 戻したエントリ（及び孤立したスナップショット実体）は消費して破棄する
  restore(turn) {
    const pts = this.points();
    if (!pts.length) return { ok: false, error: '復元ポイントがありません' };
    const t = turn ?? pts.at(-1).turn;
    if (!pts.some((p) => p.turn === t))
      return { ok: false, error: `turn ${t} の復元ポイントがありません` };
    const res = { ok: true, turn: t, restored: [], deleted: [], failed: [] };
    const byPath = new Map();   // path -> turn>=t の最古エントリ
    for (const e of this.m.entries)
      if (e.turn >= t && !byPath.has(e.path)) byPath.set(e.path, e);
    for (const e of byPath.values()) {
      try {
        if (!e.existed) { rmSync(e.path, { force: true }); res.deleted.push(e.path); }
        else {
          mkdirSync(dirname(e.path), { recursive: true });
          copyFileSync(join(this.dir, e.snap), e.path);
          res.restored.push(e.path);
        }
      } catch (err) { res.failed.push(`${e.path}: ${err.message}`); }
    }
    const kept = this.m.entries.filter((e) => e.turn < t);
    const keepSnaps = new Set(kept.map((e) => e.snap).filter(Boolean));
    this.m.entries = kept;
    for (const k of Object.keys(this.m.prompts))
      if (+k >= t) delete this.m.prompts[k];
    try {
      for (const f of readdirSync(this.dir))
        if (/^s\d+$/.test(f) && !keepSnaps.has(f)) rmSync(join(this.dir, f), { force: true });
    } catch { /* noop */ }
    this.#save();
    return res;
  }
}
