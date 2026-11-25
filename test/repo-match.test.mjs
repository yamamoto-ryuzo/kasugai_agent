// リポジトリ不整合チェック（repomatch.mjs）の単体テスト
// 実行: node --test test/
import test from 'node:test';
import assert from 'node:assert/strict';
import { suggestRepo, repoSlug } from '../src/repomatch.mjs';

const REPOS = [
  { slug: 'me/kasugai_agent', path: 'C:/devin/kasugai_agent' },
  { slug: 'me/kasugai_canvas', path: 'C:/devin/kasugai_canvas' },
  { slug: 'me/kasugai_qgis', path: 'C:/devin/kasugai_qgis' },
];
const CUR = REPOS[0];

test('repoSlug: オブジェクトと文字列の両方を受理', () => {
  assert.equal(repoSlug(CUR), 'me/kasugai_agent');
  assert.equal(repoSlug('me/x'), 'me/x');
  assert.equal(repoSlug({ github: 'g/y' }), 'g/y');
  assert.equal(repoSlug(null), null);
});

test('文中の絶対パスが他 repo 配下ならその repo を提案', () => {
  const r = suggestRepo('C:/devin/kasugai_qgis/src/main.py を直して', CUR, REPOS);
  assert.equal(r.slug, 'me/kasugai_qgis');
  assert.match(r.reason, /kasugai_qgis/);
});

test('カレント repo 配下のパス・無関係パスは提案しない', () => {
  assert.equal(suggestRepo('C:/devin/kasugai_agent/src/control.mjs を直して',
    CUR, REPOS), null);
  assert.equal(suggestRepo('src/control.mjs を直して', CUR, REPOS), null);
});

test('他 repo 名への言及のみでも提案（_/-/空白表記を吸収）', () => {
  for (const t of ['kasugai_qgis の画面を直して',
                   'kasugai-qgis を調べて', 'kasugai qgis について'])
    assert.equal(suggestRepo(t, CUR, REPOS)?.slug, 'me/kasugai_qgis', t);
});

test('カレント repo 名が文中にあれば名前言及では提案しない', () => {
  assert.equal(
    suggestRepo('kasugai_qgis の変更を kasugai_agent に取り込んで', CUR, REPOS),
    null);
});

test('複数の他 repo に言及したら判断せず null', () => {
  assert.equal(
    suggestRepo('kasugai_qgis と kasugai_canvas を比較して', CUR, REPOS), null);
});

test('無関係な文・空文は null', () => {
  assert.equal(suggestRepo('画面が重いのを直して', CUR, REPOS), null);
  assert.equal(suggestRepo('', CUR, REPOS), null);
  assert.equal(suggestRepo('kasugai_qgis を直して', CUR, []), null);
});

test('カレント repo 自身への言及のみは提案しない', () => {
  assert.equal(suggestRepo('kasugai_agent の README を更新して', CUR, REPOS),
    null);
});

test('カレント未設定でも他 repo 向けなら提案する', () => {
  const r = suggestRepo('C:/devin/kasugai_canvas/a.md を見て', null, REPOS);
  assert.equal(r.slug, 'me/kasugai_canvas');
});
