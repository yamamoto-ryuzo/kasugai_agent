// verify.rs — 実機検証（verify）フェーズの prompt・結果判定・差し戻し
// （verify.mjs 移植・純粋ロジック）
//
// 実装ターン（touched あり）の終了後に control が VERIFY_PROMPT を送り、
// エージェントは成果物を実機で起動して振る舞いを確認する。
// 報告末尾に ```kasugai-verify ブロックで結果を JSON 出力させ、control は
// extract_verdict で読み取る。ok:false なら build_back_prompt が「実装済み
// だが検証NGの課題」と「未実装の課題」に分離した差し戻し prompt を組み立て、
// verifyBackLimit 回を上限に再実装へ戻す。
use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

// 実装ターン（touched あり）の終了後に送る実機検証 prompt
pub const VERIFY_PROMPT: &str = r#"[検証] このセッションで行った実装を実機で確認してください。
- 成果物（アプリ・EXE・サービス等）を実際に起動・実行し、期待どおりに動くか確認する
- UI がある場合は Playwright 等で画面を操作し、表示・応答を確認する
- 確認した内容と結果を簡潔に報告する。問題があれば修正してから報告する
- 報告の末尾に、必ず次の形式の結果ブロックを1つだけ出力してください:

```kasugai-verify
{"ok":true}
```

対象が複数課題にまたがる場合や問題が残る場合は、実装済みの課題と未実装の課題に
分けて列挙してください（全て合格なら {"ok":true} のみで構いません）:

```kasugai-verify
{"ok":false,
 "implemented":[{"issue":"実装済みだが検証で問題のあった課題","problem":"確認した問題"}],
 "unimplemented":["未実装の課題"]}
```"#;

#[derive(Debug, Clone, Serialize)]
pub struct ImplIssue {
    pub issue: String,
    pub problem: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub ok: bool,
    pub implemented: Vec<ImplIssue>,
    pub unimplemented: Vec<String>,
}

fn verdict_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"```kasugai-verify[^\S\r\n]*\r?\n([\s\S]*?)```").unwrap()
    })
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// テキスト中の ```kasugai-verify ブロックから verdict JSON を抽出する。
/// 同一テキストに複数ある場合は最後の有効ブロックを採用。
pub fn extract_verdict(text: &str) -> Option<Verdict> {
    let mut v = None;
    for m in verdict_re().captures_iter(text) {
        let o: serde_json::Value = match serde_json::from_str(&m[1]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !o.is_object() {
            continue;
        }
        let implemented = o
            .get("implemented")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|e| {
                if let Some(s) = e.as_str() {
                    ImplIssue {
                        issue: take_chars(s.trim(), 200),
                        problem: String::new(),
                    }
                } else {
                    ImplIssue {
                        issue: take_chars(
                            e.get("issue").and_then(|v| v.as_str()).unwrap_or("").trim(),
                            200,
                        ),
                        problem: take_chars(
                            e.get("problem")
                                .or_else(|| e.get("detail"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim(),
                            400,
                        ),
                    }
                }
            })
            .filter(|e: &ImplIssue| !e.issue.is_empty())
            .collect::<Vec<_>>();
        let unimplemented = o
            .get("unimplemented")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|e| {
                take_chars(
                    e.as_str()
                        .or_else(|| e.get("issue").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .trim(),
                    200,
                )
            })
            .filter(|s: &String| !s.is_empty())
            .collect::<Vec<_>>();
        // ok 未指定の場合は列挙が空なら合格とみなす
        let ok = match o.get("ok") {
            None => implemented.is_empty() && unimplemented.is_empty(),
            Some(v) => v.as_bool() != Some(false),
        };
        v = Some(Verdict { ok, implemented, unimplemented });
    }
    v
}

/// 差し戻し prompt。verdict の課題を「実装済み（検証NG → 修正）」と
/// 「未実装（→ 実装）」に分離して指示する。
pub fn build_back_prompt(v: &Verdict) -> String {
    let mut lines = vec![
        "[差し戻し] 実機検証で対応が必要な課題が報告されました。次の区分に従って対応してください。"
            .to_string(),
    ];
    if !v.implemented.is_empty() {
        lines.push(String::new());
        lines.push("■ 実装済みの課題（検証で確認した問題を修正してください）".to_string());
        for e in &v.implemented {
            lines.push(if e.problem.is_empty() {
                format!("- {}", e.issue)
            } else {
                format!("- {}（問題: {}）", e.issue, e.problem)
            });
        }
    }
    if !v.unimplemented.is_empty() {
        lines.push(String::new());
        lines.push("■ 未実装の課題（実装してください）".to_string());
        for t in &v.unimplemented {
            lines.push(format!("- {}", t));
        }
    }
    if v.implemented.is_empty() && v.unimplemented.is_empty() {
        lines.push("検証報告の内容を確認し、問題箇所の修正・未実装分の実装を行ってください。".to_string());
    }
    lines.push(String::new());
    lines.push("完了したら内容を簡潔に報告してください（実機検証は別途自動で行われます）。".to_string());
    lines.join("\n")
}

// ---------- 独立批評パス（planning critic / diff review 共通） ----------
//
// 実装の生成系とは別コンテキストの呼び出しが ```kasugai-review ブロックで
// 批評結果を返す（manual-883d17ee 計画 critic・manual-475831d2 diff レビュー）。
// finding は severity（severe / non-severe）+ confidence（0.0〜1.0）を持ち、
// severe かつ confidence が閾値以上のものだけがゲートを止める。

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: String, // "severe" | "non-severe"（自由テキストも許容）
    pub confidence: f64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub findings: Vec<Finding>,
}

fn review_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"```kasugai-review[^\S\r\n]*\r?\n([\s\S]*?)```").unwrap()
    })
}

/// テキスト中の ```kasugai-review ブロックから批評結果を抽出する。
/// 複数ある場合は最後の有効ブロックを採用。
pub fn extract_review(text: &str) -> Option<Review> {
    let mut v = None;
    for m in review_re().captures_iter(text) {
        let o: serde_json::Value = match serde_json::from_str(&m[1]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !o.is_object() {
            continue;
        }
        let findings = o
            .get("findings")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|e| {
                let sev = e
                    .get("severity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                Finding {
                    severity: if sev.is_empty() {
                        "non-severe".into()
                    } else {
                        sev
                    },
                    confidence: e
                        .get("confidence")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.5)
                        .clamp(0.0, 1.0),
                    note: take_chars(
                        e.get("note")
                            .or_else(|| e.get("detail"))
                            .or_else(|| e.get("problem"))
                            .and_then(|v| v.as_str())
                            .or_else(|| e.as_str())
                            .unwrap_or("")
                            .trim(),
                        400,
                    ),
                }
            })
            .filter(|f: &Finding| !f.note.is_empty())
            .collect::<Vec<_>>();
        v = Some(Review { findings });
    }
    v
}

/// severe かつ confidence >= min の finding 一覧（ゲートを止める重大指摘）。
pub fn severe_findings(r: &Review, min_confidence: f64) -> Vec<&Finding> {
    r.findings
        .iter()
        .filter(|f| f.severity == "severe" && f.confidence >= min_confidence)
        .collect()
}
