// 実装待機（waiting）の単体テスト
// 実行: node --test test/
import test from 'node:test';
import assert from 'node:assert/strict';
import { AcpChat } from '../src/acp-chat.mjs';
import { normPath, extractPaths, claimsOf, findConflicts, setupClaims,
         releaseWaiters } from '../src/claims.mjs';

const CWD = 'C:/devin/kasugai_agent';
const mk = () => new AcpChat({ devinExe: 'nonexistent-devin.exe', cwd: CWD });

test('prompt 文中のファイルパスを抽出して正規化する', () => {
  const ps = extractPaths('src/control.mjs と README.md を直して', CWD);
  assert.ok(ps.includes('c:/devin/kasugai_agent/src/control.mjs'));
  assert.ok(ps.includes('c:/devin/kasugai_agent/readme.md'));
});

test('絶対パスと拡張子なし・バージョン表記の扱い', () => {
  assert.ok(extractPaths('C:/devin/x/a.mjs を見て', CWD).includes('c:/devin/x/a.mjs'));
  assert.deepEqual(extractPaths('v0.2.0 を出す', CWD), []);
});

test('normPath は相対パスを cwd 基準の絶対パスへ畳む', () => {
  assert.equal(normPath('src\\control.mjs', CWD),
    'c:/devin/kasugai_agent/src/control.mjs');
  assert.equal(normPath('C:/Devin/X/a.mjs', CWD), 'c:/devin/x/a.mjs');
});

test('クレームは busy 中のみ有効', () => {
  const a = mk();
  a.touched.add('f.mjs');
  assert.equal(claimsOf(a).size, 0);       // idle はクレームなし
  a.busy = true;
  assert.equal(claimsOf(a).size, 1);
});

test('send 前の宣言パスが他 busy chat のクレームと衝突する', () => {
  const a = mk(), b = mk();
  a.busy = true;
  setupClaims(a, 'src/control.mjs を変更して');
  const chats = new Map([[1, { chat: a }], [2, { chat: b }]]);
  const hits = findConflicts(chats, 2,
    extractPaths('src/control.mjs に追記して', b.cwd));
  assert.equal(hits.length, 1);
  assert.equal(hits[0].id, 1);
  assert.ok(hits[0].files.includes('c:/devin/kasugai_agent/src/control.mjs'));
});

test('無関係ファイルなら衝突しない・自分自身は除外', () => {
  const a = mk(), b = mk();
  a.busy = true;
  setupClaims(a, 'src/control.mjs を変更');
  const chats = new Map([[1, { chat: a }], [2, { chat: b }]]);
  assert.equal(findConflicts(chats, 2,
    extractPaths('src/acp-chat.mjs を変更', b.cwd)).length, 0);
  assert.equal(findConflicts(chats, 1,
    extractPaths('src/control.mjs を変更', a.cwd)).length, 0); // 自分は除外
});

test('hold: 競合時に waiting へ入り prompt を保留する', () => {
  const b = mk();
  b.hold('src/control.mjs に追記', { files: ['x.mjs'], blockedBy: [1] });
  assert.ok(b.waiting);
  assert.deepEqual(b.waiting.blockedBy, [1]);
  assert.equal(b.heldPrompts.length, 1);
  // 表示上は user → sys の順で残る
  assert.equal(b.messages.at(-2).role, 'user');
  assert.match(b.messages.at(-1).text, /実行待機/);
});

test('enterWait: busy 中なら session/cancel を通知する', () => {
  const b = mk();
  b.busy = true;
  b.sessionId = 's1';
  let cancelled = false;
  b.notify = (m) => { if (m === 'session/cancel') cancelled = true; };
  b.enterWait({ files: ['x.mjs'], blockedBy: [1], resume: true });
  assert.ok(b.waiting);
  assert.ok(cancelled);
  assert.equal(b.resumeOnRelease, true);
});

test('待機中の send は保留キューへ積む', async () => {
  const b = mk();
  b.hold('最初', { files: ['x.mjs'], blockedBy: [1] });
  await b.send('追加指示');
  assert.equal(b.heldPrompts.length, 2);
});

test('leaveWait: 保留 prompt を返して待機解除', () => {
  const b = mk();
  b.hold('job1', { files: ['x.mjs'], blockedBy: [1] });
  b.heldPrompts.push('job2');
  assert.equal(b.leaveWait(), 'job1\njob2');
  assert.equal(b.waiting, null);
  assert.equal(b.heldPrompts.length, 0);
});

test('leaveWait: 保留なし + resume → 中断再開の指示を返す', () => {
  const b = mk();
  b.enterWait({ files: ['x.mjs'], blockedBy: [1], resume: true });
  assert.match(b.leaveWait(), /続行/);
});

test('cancel: 待機中の中断は待機解除し保留を破棄する', async () => {
  const b = mk();
  b.hold('job', { files: ['x.mjs'], blockedBy: [1] });
  await b.cancel();
  assert.equal(b.waiting, null);
  assert.equal(b.heldPrompts.length, 0);
});

test('revise: 待機中の保留を破棄して新しい文を保留し直す', async () => {
  const b = mk();
  b.hold('old job', { files: ['x.mjs'], blockedBy: [1] });
  await b.revise('new job');
  assert.deepEqual(b.heldPrompts, ['new job']);
  assert.ok(b.waiting);                     // 待機状態自体は継続
});

test('isWriteCall / extractToolPaths', () => {
  const c = mk();
  assert.ok(c.isWriteCall({ title: 'edit file' }));
  assert.ok(!c.isWriteCall({ title: 'read file' }));
  assert.ok(c.isWriteCall({ kind: 'edit' }));
  assert.ok(!c.isWriteCall({ kind: 'read' }));
  const ps = c.extractToolPaths({
    rawInput: { file_path: 'src/x.mjs' },
    locations: [{ path: 'src/y.mjs' }] });
  assert.deepEqual(ps.sort(), ['src/x.mjs', 'src/y.mjs']);
});

test('releaseWaiters: blocker の busy 解除で待機 chat を解放して再送する', async () => {
  const a = mk(), b = mk();
  a.busy = true;
  b.hold('src/control.mjs に追記', { files: ['x.mjs'], blockedBy: [1] });
  let sent = null;
  b.send = async (t) => { sent = t; };      // ensureSession を通さず再送だけ検証
  const chats = new Map([[1, { chat: a }], [2, { chat: b }]]);
  a.busy = false;                            // blocker のターン終了
  releaseWaiters(chats, 1);
  assert.equal(b.waiting, null);
  assert.equal(sent, 'src/control.mjs に追記');
  // 再送分のクレームが宣言される
  assert.ok(b.declared.has('c:/devin/kasugai_agent/src/control.mjs'));
});

test('releaseWaiters: 残 blocker が busy なら待機継続', () => {
  const a = mk(), c = mk(), b = mk();
  a.busy = false; c.busy = true;
  b.hold('job', { files: ['x.mjs'], blockedBy: [1, 3] });
  const chats = new Map([[1, { chat: a }], [2, { chat: b }], [3, { chat: c }]]);
  releaseWaiters(chats, 1);
  assert.ok(b.waiting);
  assert.deepEqual(b.waiting.blockedBy, [3]);
});
