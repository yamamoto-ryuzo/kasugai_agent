// claims.rs — ファイルクレーム / 実装待機の競合判定（claims.mjs 移植・純粋ロジック）
//
// busy 中の chat はクレームを持つ。クレーム = declared（prompt 文中のパス）
// ∪ touched（編集系 tool_call が扱ったパス）。同一ファイルの同時編集を防ぐため、
// クレームが重なった chat は waiting（実装待機）に入り、blocker の busy が
// 解けた時点で保留 prompt を再送する。
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

use crate::util::norm_path;

// prompt 文中のファイルパス表記を拾う（拡張子で絞り、誤検出を抑える）
fn path_token() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:[A-Za-z]:[\\/])?[\w.~$\-]+(?:[\\/][\w.~$\-]+)*\.(?:mjs|cjs|js|jsx|ts|tsx|mts|cts|py|rb|go|rs|java|kt|c|h|cc|cpp|hpp|cs|html|htm|css|scss|less|json|ya?ml|toml|ini|md|markdown|txt|sh|bat|ps1|sql|vue|svelte|xml|csv)",
        )
        .unwrap()
    })
}

pub fn extract_paths(text: &str, cwd: &str) -> Vec<String> {
    let mut out = HashSet::new();
    for m in path_token().find_iter(text) {
        let n = norm_path(m.as_str(), cwd);
        if !n.is_empty() {
            out.insert(n);
        }
    }
    out.into_iter().collect()
}

/// クレームは busy 中のみ有効（待機中・idle の chat は何も握らない）
/// chat 側で busy 判定を済ませて claims 集合を渡す想定のため、
/// ここでは集合演算のみを行う。
pub struct Conflict {
    pub id: i64,
    pub files: Vec<String>,
}

/// claims_map: { chat_id -> claims(正規化済み集合) }（busy な chat のみ含める）
pub fn find_conflicts(
    claims_map: &[(i64, HashSet<String>)],
    self_id: i64,
    paths: &[String],
) -> Vec<Conflict> {
    let mut hits = Vec::new();
    if paths.is_empty() {
        return hits;
    }
    for (oid, claims) in claims_map {
        if *oid == self_id {
            continue;
        }
        let dup: Vec<String> = paths.iter().filter(|p| claims.contains(*p)).cloned().collect();
        if !dup.is_empty() {
            hits.push(Conflict { id: *oid, files: dup });
        }
    }
    hits
}
