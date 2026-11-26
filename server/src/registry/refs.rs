// registry/refs.rs — AgentOps: ルール文書（AGENTS.md 等）が参照するファイルの
// 解決（read-only。refs.mjs 移植）
// 文書内の `code` 表記からパスらしきトークンを抽出し、doc の位置 → 各 root の
// 順で実ファイルへ解決する。glob（* / ?）・ディレクトリ参照は配下のファイルへ展開。
// 秘匿系は denied として一覧には残すが、GET /api/doc の本文返却対象からは外す。
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use crate::util::{basename, dirname, home_dir, is_dir, is_file, relative_path, resolve};

// 本文を返させない秘匿系（secrets.json はパネルからも読ませない）
fn deny_res() -> &'static Vec<Regex> {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        vec![
            Regex::new(r"(?i)secret").unwrap(),
            Regex::new(r"(?i)^\.env(\.|$)").unwrap(),
            Regex::new(r"(?i)\.(pem|key|pfx|p12|kdbx)$").unwrap(),
            Regex::new(r"(?i)^id_(rsa|dsa|ecdsa|ed25519)").unwrap(),
            Regex::new(r"(?i)credential").unwrap(),
        ]
    })
}

pub fn denied(p: &str) -> bool {
    let b = basename(p);
    deny_res().iter().any(|re| re.is_match(&b))
}

pub fn skip_dirs() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| [".git", "node_modules", ".hg", ".svn"].into_iter().collect())
}

// `code` 表記のうちパスらしきものだけを抽出
fn pathy() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\w.\-~\\/*?]+$").unwrap())
}
fn looks_path(t: &str) -> bool {
    pathy().is_match(t)
        && (t.contains('/')
            || t.contains('\\')
            || t.contains(['*', '?'])
            || Regex::new(r"\.\w{1,15}$").unwrap().is_match(t))
}

fn code_tokens(text: &str) -> Vec<String> {
    let re = Regex::new(r"`([^`\r\n]+)`").unwrap();
    re.captures_iter(text)
        .map(|m| m[1].trim().to_string())
        .filter(|t| looks_path(t))
        .collect()
}

// 1セグメント分の glob マッチャ（* / ? のみ対応）
fn seg_re(seg: &str) -> Regex {
    let mut s = String::from("^");
    for c in seg.chars() {
        match c {
            '*' => s.push_str("[^\\\\/]*"),
            '?' => s.push_str("[^\\\\/]"),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                s.push('\\');
                s.push(c);
            }
            c => s.push(c),
        }
    }
    s.push('$');
    Regex::new(&format!("(?i){}", s)).unwrap()
}

// 絶対 glob パターンを展開し実在ファイルを返す（* / ? はセグメント単位）
fn expand_glob(abs_pattern: &str) -> Vec<String> {
    let mut segs: Vec<String> = abs_pattern
        .split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    // Windows ドライブ（"C:"）を維持
    let mut pre: Vec<String> = Vec::new();
    while !segs.is_empty() && !segs[0].contains(['*', '?']) {
        pre.push(segs.remove(0));
    }
    let base = if abs_pattern.starts_with('/') || abs_pattern.starts_with('\\') {
        format!("\\{}", pre.join("\\"))
    } else {
        pre.join("\\")
    };
    fn walk(dir: &Path, rest: &[String], out: &mut Vec<String>) {
        if rest.is_empty() {
            if dir.is_file() {
                out.push(dir.to_string_lossy().into_owned());
            }
            return;
        }
        if !dir.is_dir() {
            return;
        }
        let head = &rest[0];
        let tail = &rest[1..];
        if !head.contains(['*', '?']) {
            walk(&dir.join(head), tail, out);
            return;
        }
        let re = seg_re(head);
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if re.is_match(&e.file_name().to_string_lossy()) {
                    walk(&e.path(), tail, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new(&base), &segs, &mut out);
    out
}

// ディレクトリ配下のファイルを浅く列挙（深さ2・上限40。node_modules 等は除外）
fn dir_files(dir: &Path, depth: usize, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(d: &Path, lv: usize, cap: usize, out: &mut Vec<String>) {
        if out.len() >= cap {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                if out.len() >= cap {
                    return;
                }
                let p = e.path();
                if p.is_dir() {
                    if lv > 0 && !skip_dirs().contains(e.file_name().to_string_lossy().as_ref()) {
                        walk(&p, lv - 1, cap, out);
                    }
                } else if p.is_file() {
                    out.push(p.to_string_lossy().into_owned());
                }
            }
        }
    }
    walk(dir, depth, cap, &mut out);
    out
}

// 解決済みファイルのエントリ。doc 配下なら相対パス、外なら basename を表示名に
fn file_entry(p: &str, doc_dir: &str) -> Value {
    let p_n = p.replace('\\', "/");
    let d_n = doc_dir.replace('\\', "/");
    let rel = relative_path(&d_n, &p_n);
    let name = match &rel {
        Some(r) if !r.starts_with("..") => r.clone(),
        _ => basename(&p_n),
    };
    json!({"name": name, "path": p_n, "kind": "ref", "exists": true, "denied": denied(&p_n)})
}

// トークンを実パスへ解決。bases の先頭から順に探し最初に実在した場所を採用
fn resolve_token(tok: &str, bases: &[String], doc_dir: &str) -> Vec<Value> {
    let t = if let Some(rest) = tok.strip_prefix('~') {
        format!("{}{}", home_dir().to_string_lossy().replace('\\', "/"), rest)
    } else {
        tok.to_string()
    };
    let globby = t.contains(['*', '?']);
    for base in bases {
        let abs = resolve(base, &t);
        if globby {
            let hits = expand_glob(&abs.replace('/', "\\"));
            if !hits.is_empty() {
                return hits.iter().map(|p| file_entry(p, doc_dir)).collect();
            }
            continue;
        }
        if is_file(&abs) {
            return vec![file_entry(&abs, doc_dir)];
        }
        if is_dir(&abs) {
            let kids = dir_files(Path::new(&abs), 2, 40);
            if !kids.is_empty() {
                return kids.iter().map(|p| file_entry(p, doc_dir)).collect();
            }
            return vec![json!({"name": tok, "path": abs, "kind": "ref",
                "exists": true, "isDir": true, "denied": denied(&abs)})];
        }
    }
    let guess = resolve(bases.first().map(|s| s.as_str()).unwrap_or("."), &t);
    vec![json!({"name": tok, "path": guess, "kind": "ref",
        "exists": false, "denied": denied(&guess)})]
}

// docPath の文書が参照するファイル一覧。roots は解決フォールバック
pub fn list_ref_docs(doc_path: &str, roots: &[String], scope: &str) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(doc_path) else {
        return Vec::new();
    };
    let doc_dir = dirname(&resolve(".", doc_path));
    let mut bases = vec![doc_dir.clone()];
    bases.extend(roots.iter().filter(|r| !r.is_empty()).map(|r| resolve(".", r)));
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in code_tokens(&text) {
        for mut e in resolve_token(&tok, &bases, &doc_dir) {
            let key = e["path"].as_str().unwrap_or("").to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            e["scope"] = json!(scope);
            out.push(e);
        }
    }
    out
}

// GET /api/doc の返却許可リスト用: 実在・非秘匿のファイルパス（小文字化済）
pub fn list_all_ref_paths(docs: &[Value], roots: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    for d in docs {
        let path = d["path"].as_str().unwrap_or("");
        for r in list_ref_docs(path, roots, "") {
            if r["exists"].as_bool() == Some(true)
                && r["denied"].as_bool() != Some(true)
                && r["isDir"].as_bool() != Some(true)
            {
                let p = r["path"].as_str().unwrap_or("");
                if is_file(p) {
                    out.insert(p.to_lowercase());
                }
            }
        }
    }
    out
}
