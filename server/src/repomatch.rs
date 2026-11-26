// repomatch.rs — チャット内容とカレントリポジトリの不整合検出（repomatch.mjs 移植）
//
// send 前に prompt を検査し、「この内容は別の登録 repo 向け」と判断できる場合に
// 切替候補 {slug, path, reason} を返す。手がかりは2段:
//   強: prompt 文中のファイルパスが他 repo の path 配下に解決される
//   弱: 他 repo の名前（slug 名部 / path ベース名）への言及がある
use regex::Regex;
use serde_json::Value;

use crate::claims::extract_paths;
use crate::util::{basename, norm_path, norm_slashes};

pub fn repo_slug(r: Option<&Value>) -> Option<String> {
    let r = r?;
    r.get("slug")
        .or_else(|| r.get("github"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn repo_path(r: &Value) -> String {
    norm_path(r.get("path").and_then(|v| v.as_str()).unwrap_or(""), ".")
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Suggest {
    pub slug: Option<String>,
    pub path: String,
    pub reason: String,
}

// 名前トークン: slug の名部（owner/name の name）と path のベース名。
// 4文字未満は誤検出が多いので捨てる
fn name_tokens(r: &Value) -> Vec<String> {
    let mut out = std::collections::HashSet::new();
    let slug = repo_slug(Some(r)).unwrap_or_default();
    let sn = slug.rsplit('/').next().unwrap_or("");
    let pn = basename(&norm_slashes(
        r.get("path").and_then(|v| v.as_str()).unwrap_or(""),
    ));
    for t in [sn, &pn] {
        if t.chars().count() >= 4 {
            out.insert(t.to_lowercase());
        }
    }
    out.into_iter().collect()
}

// 名前の言及判定。トークン中の _ / - は 空白・_・- いずれの表記でも許す。
// Rust regex は lookbehind/lookahead 非対応のため、境界は包含マッチで代用
// （is_match の真偽のみ使うので境界文字の混入は問題ない）
fn esc_re(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn mention_re(tok: &str) -> Regex {
    let stripped: String = tok
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect();
    let body = stripped
        .chars()
        .map(|c| format!("{}[\\s_-]*", esc_re(&c.to_string())))
        .collect::<String>();
    Regex::new(&format!("(^|[^\\w-]){}([^\\w-]|$)", body)).unwrap()
}

fn mentions(text: &str, tok: &str) -> bool {
    mention_re(tok).is_match(text)
}

/// text が cur（chat のカレント repo）以外の repo 向けに見えるなら
/// Suggest を返す。判断できなければ None
pub fn suggest_repo(text: &str, cur: Option<&Value>, repos: &[Value]) -> Option<Suggest> {
    let t = text.trim();
    if t.is_empty() || repos.is_empty() {
        return None;
    }
    let cur_slug = cur.and_then(|r| repo_slug(Some(r)));
    let cur_path = cur.map(|r| repo_path(r)).unwrap_or_default();
    let others: Vec<&Value> = repos
        .iter()
        .filter(|r| repo_slug(Some(r)) != cur_slug && repo_path(r) != cur_path)
        .collect();
    if others.is_empty() {
        return None;
    }

    // 強: 文中のパスが他 repo の path 配下に解決される
    for p in extract_paths(t, if cur_path.is_empty() { "." } else { &cur_path }) {
        if let Some(hit) = others.iter().find(|r| {
            let rp = repo_path(r);
            !rp.is_empty() && (p == rp || p.starts_with(&format!("{}/", rp)))
        }) {
            let tail = p
                .rsplit('/')
                .take(2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("/");
            return Some(Suggest {
                slug: repo_slug(Some(hit)),
                path: hit.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                reason: format!(
                    "パス {} が {} 配下",
                    tail,
                    repo_slug(Some(hit)).unwrap_or_else(|| repo_path(hit))
                ),
            });
        }
    }

    // 弱: 他 repo 名への言及。カレント repo 名も出る文は提案しない
    if let Some(c) = cur {
        if name_tokens(c).iter().any(|tok| mentions(t, tok)) {
            return None;
        }
    }
    let hits: Vec<&&Value> = others
        .iter()
        .filter(|r| name_tokens(r).iter().any(|tok| mentions(t, tok)))
        .collect();
    if hits.len() != 1 {
        return None;
    }
    let h = hits[0];
    let tok = name_tokens(h).into_iter().find(|x| mentions(t, x))?;
    Some(Suggest {
        slug: repo_slug(Some(h)),
        path: h.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        reason: format!("「{}」への言及", tok),
    })
}
