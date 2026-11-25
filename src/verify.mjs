// 実機検証（verify）フェーズの prompt・結果判定・差し戻し（純粋ロジック・副作用なし）
//
// 実装ターン（touched あり）の終了後に control が VERIFY_PROMPT を送り、
// エージェントは成果物を実機で起動して振る舞いを確認する（README §5.5）。
// 報告末尾に ```kasugai-verify ブロックで結果を JSON 出力させ、control は
// extractVerdict で読み取る。ok:false なら buildBackPrompt が「実装済み
// だが検証NGの課題」と「未実装の課題」に分離した差し戻し prompt を組み立て、
// verifyBackLimit 回を上限に再実装へ戻す（遷移・上限判定は control.mjs 側）

// 実装ターン（touched あり）の終了後に送る実機検証 prompt。
// 検証 = 対象EXE自体を実機で起動し Playwright 等で画面・応答を駆動確認する
// 「実装の確認」。問題があれば修正まで行わせ、検証中の修正は retryLimit 内で
// 再検証する（verifyCount）
export const VERIFY_PROMPT = `[検証] このセッションで行った実装を実機で確認してください。
- 成果物（アプリ・EXE・サービス等）を実際に起動・実行し、期待どおりに動くか確認する
- UI がある場合は Playwright 等で画面を操作し、表示・応答を確認する
- 確認した内容と結果を簡潔に報告する。問題があれば修正してから報告する
- 報告の末尾に、必ず次の形式の結果ブロックを1つだけ出力してください:

\`\`\`kasugai-verify
{"ok":true}
\`\`\`

対象が複数課題にまたがる場合や問題が残る場合は、実装済みの課題と未実装の課題に
分けて列挙してください（全て合格なら {"ok":true} のみで構いません）:

\`\`\`kasugai-verify
{"ok":false,
 "implemented":[{"issue":"実装済みだが検証で問題のあった課題","problem":"確認した問題"}],
 "unimplemented":["未実装の課題"]}
\`\`\``;

// テキスト中の ```kasugai-verify ブロックから verdict JSON を抽出する。
// 同一テキストに複数ある場合は最後の有効ブロックを採用。壊れた JSON・
// オブジェクトでないものは捨てる（パース不能で処理を止めない）。
// 返り値: { ok, implemented:[{issue,problem}], unimplemented:[string] } | null
const VERDICT_RE = /```kasugai-verify[^\S\r\n]*\r?\n([\s\S]*?)```/g;

export function extractVerdict(text) {
  let v = null;
  for (const m of String(text ?? '').matchAll(VERDICT_RE)) {
    let o;
    try { o = JSON.parse(m[1]); } catch { continue; }
    if (!o || typeof o !== 'object' || Array.isArray(o)) continue;
    const implemented = (Array.isArray(o.implemented) ? o.implemented : [])
      .map((e) => typeof e === 'string'
        ? { issue: e.trim().slice(0, 200), problem: '' }
        : { issue: String(e?.issue ?? '').trim().slice(0, 200),
            problem: String(e?.problem ?? e?.detail ?? '').trim().slice(0, 400) })
      .filter((e) => e.issue);
    const unimplemented = (Array.isArray(o.unimplemented) ? o.unimplemented : [])
      .map((e) => String(typeof e === 'string' ? e : e?.issue ?? '').trim()
        .slice(0, 200))
      .filter(Boolean);
    // ok 未指定の場合は列挙が空なら合格とみなす
    const ok = o.ok === undefined
      ? !(implemented.length || unimplemented.length)
      : o.ok !== false;
    v = { ok, implemented, unimplemented };
  }
  return v;
}

// 差し戻し prompt。verdict の課題を「実装済み（検証NG → 修正）」と
// 「未実装（→ 実装）」に分離して指示する。列挙が無い NG でも
// 報告の確認を促す汎用文へ落とす
export function buildBackPrompt(v) {
  const lines =
    ['[差し戻し] 実機検証で対応が必要な課題が報告されました。次の区分に従って対応してください。'];
  if (v?.implemented?.length) {
    lines.push('', '■ 実装済みの課題（検証で確認した問題を修正してください）');
    for (const e of v.implemented)
      lines.push(`- ${e.issue}${e.problem ? `（問題: ${e.problem}）` : ''}`);
  }
  if (v?.unimplemented?.length) {
    lines.push('', '■ 未実装の課題（実装してください）');
    for (const t of v.unimplemented) lines.push(`- ${t}`);
  }
  if (!v?.implemented?.length && !v?.unimplemented?.length)
    lines.push('検証報告の内容を確認し、問題箇所の修正・未実装分の実装を行ってください。');
  lines.push('', '完了したら内容を簡潔に報告してください（実機検証は別途自動で行われます）。');
  return lines.join('\n');
}
