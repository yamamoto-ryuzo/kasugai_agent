// 検証 verdict 解析・差し戻し prompt の単体テスト
// 実行: node --test test/
import test from 'node:test';
import assert from 'node:assert/strict';
import { VERIFY_PROMPT, extractVerdict, buildBackPrompt } from '../src/verify.mjs';

test('検証 prompt は kasugai-verify ブロックの出力を指示する', () => {
  assert.ok(VERIFY_PROMPT.includes('```kasugai-verify'));
  assert.ok(VERIFY_PROMPT.includes('"unimplemented"'));
});

test('ok:true の verdict を読み取る', () => {
  const v = extractVerdict('確認しました。\n```kasugai-verify\n{"ok":true}\n```');
  assert.deepEqual(v, { ok: true, implemented: [], unimplemented: [] });
});

test('NG verdict は実装済み/未実装へ分離する', () => {
  const text = '一部NGでした。\n```kasugai-verify\n' +
    '{"ok":false,' +
    '"implemented":[{"issue":"ログイン画面","problem":"ボタン無反応"},"一覧画面"],' +
    '"unimplemented":["CSV出力"]}\n```';
  const v = extractVerdict(text);
  assert.equal(v.ok, false);
  assert.deepEqual(v.implemented, [
    { issue: 'ログイン画面', problem: 'ボタン無反応' },
    { issue: '一覧画面', problem: '' }]);
  assert.deepEqual(v.unimplemented, ['CSV出力']);
});

test('同一メッセージに複数ブロックがあれば最後を採用', () => {
  const text = '```kasugai-verify\n{"ok":false,"unimplemented":["A"]}\n```\n' +
    '途中報告\n```kasugai-verify\n{"ok":true}\n```';
  assert.equal(extractVerdict(text).ok, true);
});

test('壊れた JSON・ブロック無しは null（処理を止めない）', () => {
  assert.equal(extractVerdict('```kasugai-verify\n{broken\n```'), null);
  assert.equal(extractVerdict('結果だけの普通の報告'), null);
  assert.equal(extractVerdict(null), null);
  // 配列・スカラは verdict とみなさない
  assert.equal(extractVerdict('```kasugai-verify\n[]\n```'), null);
});

test('ok 未指定は列挙の有無で合格/NG を推定する', () => {
  assert.equal(extractVerdict('```kasugai-verify\n{}\n```').ok, true);
  const ng = extractVerdict('```kasugai-verify\n{"unimplemented":["X"]}\n```');
  assert.equal(ng.ok, false);
});

test('差し戻し prompt は実装済みと未実装を分離する', () => {
  const p = buildBackPrompt({
    ok: false,
    implemented: [{ issue: 'ログイン画面', problem: 'ボタン無反応' }],
    unimplemented: ['CSV出力'] });
  assert.ok(p.includes('実装済み'));
  assert.ok(p.includes('未実装'));
  assert.ok(p.includes('ログイン画面'));
  assert.ok(p.includes('ボタン無反応'));
  assert.ok(p.includes('CSV出力'));
  assert.ok(p.indexOf('実装済み') < p.indexOf('未実装'));
});

test('列挙なしの NG でも汎用の差し戻し文になる', () => {
  const p = buildBackPrompt({ ok: false, implemented: [], unimplemented: [] });
  assert.ok(p.includes('[差し戻し]'));
  assert.ok(p.includes('検証報告'));
});
