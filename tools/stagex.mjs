// stage 分割コミット補助: HEAD + 自分の変更のみを再構成して index へ stage する。
// 使い方: node tools/stagex.mjs <stage> [--report] [files...]
//   stage = 1..4（1=分割起票 2=スナップショット 3=親課題リンク 4=事案）
// unit は内容正規表現（when）で特定する（hunk index は相手のコミットでずれるため）
import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const STAGE = Number(process.argv[2] || 0);
const REPORT = process.argv.includes('--report');
const BASE = process.env.STAGEX_BASE || 'HEAD'; // 並走セッションのコミット競合対策: 明示 sha 固定

const sh = (cmd, opts = {}) =>
  execSync(cmd, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, ...opts });
const head = (f) => sh(`git show ${BASE}:"${f}"`);
const diff0 = (f) => sh(`git diff ${BASE} -U0 -- "${f}"`);

function parseDiff(text) {
  const lines = text.split('\n');
  const hunks = [];
  let cur = null;
  for (const ln of lines) {
    const m = ln.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
    if (m) {
      cur = { a: +m[1], ac: +(m[2] ?? 1), b: +m[3], bc: +(m[4] ?? 1), lines: [] };
      hunks.push(cur);
      continue;
    }
    if (!cur) continue;
    if (/^[+-\\]/.test(ln)) cur.lines.push(ln);
  }
  for (const h of hunks) {
    h.units = [];
    let i = 0;
    while (i < h.lines.length) {
      const rm = [];
      while (i < h.lines.length && h.lines[i].startsWith('-'))
        rm.push(h.lines[i++].slice(1));
      const add = [];
      while (i < h.lines.length && h.lines[i].startsWith('+'))
        add.push(h.lines[i++].slice(1));
      if (h.lines[i]?.startsWith('\\')) i++;
      if (rm.length || add.length) h.units.push({ rm, add });
    }
  }
  return hunks;
}

const K = { split: 1, snap: 2, link: 3, inc: 4, theirs: 0 };

function autoStage(u, order) {
  const t = u.add.concat(u.rm).join('\n');
  for (const [re, s] of order) if (re.test(t)) return s;
  return -1;
}

// ルール: when（unit rm+add 結合テキストに対する正規表現）に最初に一致した
// ルールを適用。action は以下のいずれか:
//   stage:N          unit 全体を stage N へ
//   segs+pre         add[] をマーカー行で分割し各 segment に stage を割当
//   stageLines:{N:..} stage 別の出力行（floor lookup、最小 stage 未満は rm）
const RULES = {
  'server/src/pipeline.rs': [
    { when: /parent_issue_link|split_issue_files|split_items_detects/, segs: [{ m: /fn \w*split/, stage: K.split }], pre: K.link },
    { when: /分割起票（manual/, segs: [{ m: /分割起票（manual/, stage: K.split }], pre: K.snap },
    { when: /閾値超の大規模課題は splitProposal/, stageLines: {
        1: [
          '/// 閾値超の大規模課題は splitProposal 付きで gated へ（manual-0577d650）。',
          'fn assess(cfg: &Value, issues: &[Value], issue: &mut Value) {',
        ],
      },
    },
    { when: /大規模課題の検出（manual-0577d650。o 借用前に判定する/, segs: [{ m: /大規模課題の検出/, stage: K.split }], pre: K.theirs },
    { when: /大規模課題の分割提案/, stage: K.split },
    { when: /cfg 未指定のプレビューは既定閾値/, stage: K.split },
    { when: /oversized\(cfg|fn split_items/, stage: K.split },
    { when: /Linked\(Value\)|IntakeOutcome::Linked/, stage: K.link },
    { when: /親課題リンク（manual-488ea7f9/, stage: K.link },
    { when: /intake スナップショット（intake-snapshot/, stage: K.snap },
    { when: /add_followup|consume_followups|intake_freezes_snapshot/, stage: K.snap },
    { when: /事案集約（incidents/, stage: K.inc },
    { when: /assess\(&cfg, &issues, &mut issue\)/, stageLines: {
        1: ['        assess(&cfg, &issues, &mut issue);'],
      },
    },
  ],
  'server/src/runner.rs': [
    { when: /consume_followups|consumed 化/, stage: K.snap },
    { when: /snap_get|intakeSnapshot/, stage: K.snap },
    { when: /起票後の追記（followUps/, segs: [
        { m: /起票後の追記（followUps/, stage: K.snap },
        { m: /事案（incidents/, stage: K.inc },
        { m: /計画承認ゲート/, stage: K.theirs },
      ], pre: K.theirs },
    { when: /事案（incidents/, segs: [
        { m: /事案（incidents/, stage: K.inc },
        { m: /計画承認ゲート/, stage: K.theirs },
      ], pre: K.theirs },
    { when: /issue_trailer/, segs: [
        { m: /crate::util::issue_trailer/, stage: K.link },
        { m: /独立 diff レビュー|diff レビュー/, stage: K.theirs },
      ], pre: K.theirs },
    { when: /build_prompt_uses_intake_snapshot/, stage: K.snap },
    { when: /plan_approve|plan_reject|trajectory|audit::|tstep|Obs|traj|lessons|knowledgeRefs|attach|case|critic|metrics|heartbeat|consolidat|eval|replay|usage|runbook|watch/, stage: K.theirs },
  ],
  'server/src/control.rs': [
    { when: /api\/issues\/\{key\}\/\{approve/, stageLines: {
        1: ['// POST /api/issues/{key}/{approve|reject|requeue|push-approve|dispatch|split}'],
        2: ['// POST /api/issues/{key}/{approve|reject|requeue|push-approve|dispatch|followup|split}'],
      },
    },
    { when: /followup は \{text\} を課題の followUps/, segs: [{ m: /split は/, stage: K.split }], pre: K.snap },
    { when: /b: Bytes,/, stage: K.split },
    { when: /followup は body/, segs: [{ m: /split は/, stage: K.split }], pre: K.snap },
    { when: /incidents（事案集約/, stage: K.inc },
    { when: /api\/incidents/, segs: [{ m: /api\/incidents/, stage: K.inc }], pre: K.theirs },
  ],
  'server/src/main.rs': [
    { when: /mod incidents/, stage: K.inc },
    { when: /mod /, stage: K.theirs },
  ],
  'server/src/util.rs': [{ when: /issue_trailer/, stage: K.link }],
  'server/src/detectors/ci_failure.rs': [{ when: /./, stage: K.link }],
  'server/src/detectors/mod.rs': [{ when: /./, stage: K.link }],
  'panel/index.html': [
    { when: /async function issueFollowup/, segs: [
        { m: /async function issueFollowup/, stage: K.snap, back: true },
        { m: /async function issueSplit/, stage: K.split, back: true },
        { m: /async function issueMerge/, stage: K.inc, back: true },
      ], pre: K.theirs },
    { when: /事案操作/, segs: [{ m: /事案操作/, stage: K.inc }], pre: K.split },
    { when: /it\.incident/, segs: [{ m: /it\.incident/, stage: K.inc }], pre: K.split },
    { when: /box\.innerHTML/, stageLines: {
        2: ['  box.innerHTML = renderItemHead(i, true) + info + plan + members + runs + fuCard + renderItemBody(i.body);'],
        4: ['  box.innerHTML = renderItemHead(i, true) + info + plan + members + incCard + runs + fuCard + renderItemBody(i.body);'],
      },
    },
    { when: /fuCard|followUps（起票後/, stage: K.snap },
    { when: /incident|incList|incById|incEmitted|issueRow|margin-left|\binc\b/, stage: K.inc },
    { when: /splitProposal|iss\.split|issueSplit|分割提案/, stage: K.split },
    { when: /./, stage: K.theirs },
  ],
  'README.md': [
    { when: /#### 5\.2\.1/, segs: [{ m: /#### 5\.2\.1/, stage: K.inc }], pre: K.split },
    { when: /### 5\.[0-9]|trajectory|観測|監査|知見|計画|planGate|レッスン|メトリクス/, stage: K.theirs },
    { when: /intake スナップショット/, stage: K.snap },
    { when: /parentIssue|Issue:トレーラ|親課題|linkedIssue/, stage: K.link },
    { when: /PAR\{|LINK\[/, stage: K.link },
    { when: /incident|事案|incidents\.json/, stage: K.inc },
    { when: /分割|splitProposal|splitMax|oversized|splitInto/, stage: K.split },
    { when: /./, stage: K.theirs },
  ],
  'diagrams/issue-lifecycle.mmd': [
    { when: /split|分割|splitInto/, stage: K.split },
    { when: /./, stage: K.theirs },
  ],
  'diagrams/detect-pipeline.mmd': [{ when: /./, stage: K.link }],
  'diagrams/docs/3-1-diagrams.md': [
    { when: /parentIssue|Issue:|PAR|LINK|帰属/, stage: K.link },
    { when: /split|分割/, stage: K.split },
    { when: /./, stage: K.theirs },
  ],
  'docs/3-1-diagrams.html': [
    { when: /parentIssue|Issue:|PAR|LINK|帰属/, stage: K.link },
    { when: /split|分割/, stage: K.split },
    { when: /./, stage: K.theirs },
  ],
};

const JSON_FILES = {
  'config.example.json': (k) =>
    /splitMax/.test(k) ? K.split : /incidentWindow/.test(k) ? K.inc : K.theirs,
};
for (const lang of ['ja', 'en', 'zh', 'ko', 'es', 'fr', 'de', 'pt']) {
  JSON_FILES[`panel/i18n/${lang}.json`] = (k) =>
    /plan|^st\.det/.test(k)
      ? K.theirs
      : /followup|snapshot|^iss\.fu/.test(k)
        ? K.snap
        : /inc/i.test(k)
          ? K.inc
          : /split/i.test(k)
            ? K.split
            : K.theirs;
}

function floorLookup(map, stage) {
  const keys = Object.keys(map).map(Number).sort((a, b) => a - b);
  let best = null;
  for (const k of keys) if (k <= stage) best = k;
  return best === null ? null : map[best];
}

function applyRule(u, rule, stage) {
  if (rule.stageLines) {
    return {
      emit: floorLookup(rule.stageLines, stage) ?? u.rm,
      tag: `stageLines(${stage})`,
    };
  }
  if (rule.segs) {
    const bounds = new Map();
    for (const [i, ln] of u.add.entries()) {
      for (const sg of rule.segs) if (sg.m.test(ln)) bounds.set(i, sg.stage);
    }
    for (const sg of rule.segs.filter((s) => s.back)) {
      const i0 = [...bounds.entries()].find(([, v]) => v === sg.stage)?.[0];
      if (i0 === undefined) continue;
      let j = i0 - 1;
      while (
        j >= 0 &&
        !bounds.has(j) &&
        (u.add[j].trim() === '' || /^\s*(\/\/|#|<!--)/.test(u.add[j]))
      ) {
        bounds.set(j, sg.stage);
        j--;
      }
    }
    const sorted = [...bounds.entries()].sort((a, b) => a[0] - b[0]);
    const parts = [];
    let cur = { stage: rule.pre, lines: [] };
    let si = 0;
    for (const [i, ln] of u.add.entries()) {
      if (si < sorted.length && i === sorted[si][0]) {
        parts.push(cur);
        cur = { stage: sorted[si][1], lines: [] };
        si++;
      }
      cur.lines.push(ln);
    }
    parts.push(cur);
    const kept = parts.filter((p) => p.stage > 0 && p.stage <= stage);
    return {
      emit: kept.length ? kept.flatMap((p) => p.lines) : u.rm,
      tag: `segs(${parts.map((p) => `${p.stage}:${p.lines.length}`).join(',')})`,
    };
  }
  const st = rule.stage;
  if (st === 0 || st > stage) return { emit: u.rm, tag: `theirs(${st})` };
  return { emit: u.add, tag: `stage${st}` };
}

function buildFile(path, stage) {
  const rules = RULES[path];
  if (!rules) return null;
  const old = head(path).split('\n');
  const hunks = parseDiff(diff0(path));
  const out = [];
  let pos = 0;
  const report = [];
  hunks.forEach((h, hi) => {
    // @@ -A,0 （純粋挿入）は「行 A の後」に挿入するため、保持する前置行は
    // h.ac=0 なら A 行分、rm あり hunk なら A-1 行分（rm 行は後で skip）
    const before = h.ac === 0 ? h.a : h.a - 1;
    out.push(...old.slice(pos, before));
    pos = before + h.ac;
    for (const u of h.units) {
      const text = u.add.concat(u.rm).join('\n');
      const rule = rules.find((r) => r.when.test(text));
      let emit, tag;
      if (rule) {
        ({ emit, tag } = applyRule(u, rule, stage));
      } else {
        emit = u.rm;
        tag = 'NOMATCH→theirs';
      }
      out.push(...emit);
      report.push(
        `  h${hi} u[rm=${u.rm.length},add=${u.add.length}] -> ${tag}: ${(u.add[0] || u.rm[0] || '').slice(0, 55)}`
      );
    }
  });
  out.push(...old.slice(pos));
  return { text: out.join('\n'), report };
}

function buildJsonFile(path, stage) {
  const classify = JSON_FILES[path];
  const headText = head(path);
  const base = JSON.parse(headText);
  const wt = JSON.parse(readFileSync(path, 'utf8'));
  const report = [];
  const adds = [];
  for (const [k, v] of Object.entries(wt)) {
    if (k in base) continue;
    const s = classify(k);
    if (s > 0 && s <= stage) {
      adds.push(`  ${JSON.stringify(k)}: ${JSON.stringify(v)}`);
      report.push(`  +key ${k} (stage${s})`);
    }
  }
  if (!adds.length) return { text: headText, report };
  // minimal diff: insert new keys just before the closing brace of HEAD text
  const lines = headText.split('\n');
  let i = lines.length - 1;
  while (i >= 0 && lines[i].trim() === '') i--;
  // lines[i] should be the final '}' ; lines[i-1] is the last entry
  let prev = i - 1;
  if (prev >= 0 && !lines[prev].trimEnd().endsWith(',')) {
    lines[prev] = lines[prev].trimEnd() + ',';
  }
  adds.forEach((a, n) => lines.splice(i + n, 0, n === adds.length - 1 ? a : a + ','));
  return { text: lines.join('\n'), report };
}

const FILES = process.argv.slice(3).filter((a) => !a.startsWith('--'));
const targets = FILES.length
  ? FILES
  : [...Object.keys(RULES), ...Object.keys(JSON_FILES)];

for (const f of targets) {
  const r = JSON_FILES[f] ? buildJsonFile(f, STAGE) : buildFile(f, STAGE);
  if (!r) continue;
  console.log(`== ${f}`);
  if (REPORT && r.report.length) console.log(r.report.join('\n'));
  if (!REPORT) {
    const oid = sh(`git hash-object -w --stdin`, { input: r.text }).trim();
    sh(`git update-index --cacheinfo 100644,${oid},"${f}"`);
    console.log(`  staged ${oid.slice(0, 10)}`);
  }
}
