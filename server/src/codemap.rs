// codemap.rs — AST 単位の構造的コンテキスト取得
// （正本: diagrams/codemap.mmd、manual-38bbe09e、README §5.4）
//
// AutoCodeRover の structure-aware code search に倣い、dispatch prompt へ
// 渡すコード文脈を「ファイル全体」ではなく関数・impl・class 等の構造単位
// （行範囲）で取得する。外部パーサ（tree-sitter 等）は持たず、言語別の
// 軽量パターンで単位の開始行を検出し、ブレース深さ・インデント幅で
// 終端行を特定する（依存追加ゼロの実用実装）。
//
// 課題 title+body から抽出したトークンと単位名・パスを照合して上位
// maxUnits 件を「## 関連コード構造（AST 単位）」節として組み立てる。
// ヒット 0 件・codeMap.enabled=false・repo 不在は None（注入しない）。
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::cfg_obj;

/// 構造単位（関数・impl・class 等の行範囲。1-based 行番号）
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub file: String, // repo 相対パス（/ 区切り）
    pub kind: String, // fn|impl|struct|enum|trait|mod|def|class|function|arrow
    pub name: String,
    pub start: usize,
    pub end: usize,
}

// ---------- 言語別の単位抽出 ----------

fn lang_of(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("py") => Some("python"),
        Some("js") | Some("mjs") | Some("cjs") | Some("jsx") => Some("js"),
        Some("ts") | Some("tsx") | Some("mts") | Some("cts") => Some("js"),
        _ => None,
    }
}

/// 単位の開始行パターン（言語ごとの軽量マッチ。戻り値 (kind, name)）
fn unit_start(lang: &str, line: &str) -> Option<(String, String)> {
    let t = line.trim_start();
    match lang {
        "rust" => rust_unit(t),
        "python" => py_unit(t),
        _ => js_unit(t),
    }
}

/// 宣言トークンから単位名を取る（`helper()`→`helper`、
/// `fetch_data(url):`→`fetch_data`、`inner;`→`inner`）
fn head_name(n: &str) -> String {
    n.split(|c: char| "({;<:=>".contains(c))
        .next()
        .unwrap_or("")
        .to_string()
}

fn rust_unit(t: &str) -> Option<(String, String)> {
    // コメント・属性行は単位開始にしない
    if t.starts_with("//") || t.starts_with("#") || t.starts_with('*') {
        return None;
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    // 修飾子を剥がして先頭キーワードを探す
    fn strip<'a>(w: &'a str) -> &'a str {
        w.trim_start_matches("pub(")
            .trim_start_matches("pub")
            .trim_start_matches("crate")
            .trim_start_matches("async")
            .trim_start_matches("unsafe")
            .trim_start_matches('(')
            .trim_start_matches(')')
    }
    for (i, w) in words.iter().enumerate() {
        let w = strip(w);
        for k in ["fn", "impl", "struct", "enum", "trait", "mod"] {
            if w == k {
                // 名前は直後のトークン（impl X / fn name / mod name）
                let name = words
                    .get(i + 1)
                    .map(|n| head_name(n))
                    .unwrap_or_default();
                if name.is_empty() {
                    return None;
                }
                return Some((k.to_string(), name));
            }
        }
        // fn/struct 等は宣言の先頭語だけを見る（途中の語は拾わない）
        if i >= 2 {
            return None;
        }
    }
    None
}

fn py_unit(t: &str) -> Option<(String, String)> {
    if t.starts_with('#') {
        return None;
    }
    let mut w = t.split_whitespace();
    let head = w.next()?;
    let (kind, rest) = match head {
        "def" => ("def", w.next()),
        "async" if w.next() == Some("def") => ("def", w.next()),
        "class" => ("class", w.next()),
        _ => return None,
    };
    let name = head_name(rest?);
    if name.is_empty() {
        return None;
    }
    Some((kind.to_string(), name))
}

fn js_unit(t: &str) -> Option<(String, String)> {
    if t.starts_with("//") || t.starts_with('*') || t.starts_with("/*") {
        return None;
    }
    // export / async / const 等の修飾を順に剥がす
    let mut s = t;
    for pref in ["export default ", "export ", "declare "] {
        if let Some(r) = s.strip_prefix(pref) {
            s = r;
        }
    }
    if let Some(r) = s.strip_prefix("async ") {
        s = r;
    }
    let mut w = s.split_whitespace();
    match w.next()? {
        "function" => {
            let n = w.next().unwrap_or("").trim_start_matches('*');
            let name = head_name(n);
            if name.is_empty() {
                None
            } else {
                Some(("function".into(), name))
            }
        }
        "class" => {
            let name = head_name(w.next().unwrap_or(""));
            if name.is_empty() {
                None
            } else {
                Some(("class".into(), name))
            }
        }
        "const" | "let" | "var" => {
            // const name = (...) => / const name = async ( / const name = function
            let name = head_name(w.next().unwrap_or(""));
            if name.is_empty()
                || !name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
                    .unwrap_or(false)
                || !s.contains("=>")
            {
                return None;
            }
            Some(("arrow".into(), name))
        }
        _ => None,
    }
}

/// ブレース言語の単位終端: 開始行から `{`/`}` の深さを追い、最初に深さ 0
/// へ戻った行を終端とする。`{` が一度も出ない宣言（`mod x;` 等）は
/// 開始行自身を終端とする。暴走防止に最大 MAX_UNIT_LINES 行で打切る
fn brace_end(lines: &[String], start: usize) -> usize {
    const MAX_UNIT_LINES: usize = 400;
    let mut depth: i64 = 0;
    let mut seen_open = false;
    let mut in_block_comment = false;
    for (i, l) in lines.iter().enumerate().skip(start - 1) {
        let mut in_str: Option<char> = None;
        let mut chars = l.chars().peekable();
        let mut j = 0usize;
        while let Some(c) = chars.next() {
            j += 1;
            if in_block_comment {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                }
                continue;
            }
            if let Some(q) = in_str {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    in_str = None;
                }
                continue;
            }
            match c {
                '"' | '\'' | '`' => in_str = Some(c),
                '/' if chars.peek() == Some(&'/') => break, // 行コメント
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    in_block_comment = true;
                }
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' => {
                    depth -= 1;
                    if seen_open && depth <= 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            let _ = j;
        }
        if i + 1 - start >= MAX_UNIT_LINES {
            return i + 1;
        }
        if !seen_open && l.trim_end().ends_with(';') {
            return i + 1; // 中身を持たない宣言（mod x; 等）
        }
    }
    lines.len()
}

/// Python の単位終端: def/class のインデント幅以下に戻る最初の
/// 非空行（コメント行は無視）を終端とする
fn indent_end(lines: &[String], start: usize) -> usize {
    let base = lines[start - 1].chars().take_while(|c| c.is_whitespace()).count();
    let mut end = start;
    for (i, l) in lines.iter().enumerate().skip(start) {
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let ind = l.chars().take_while(|c| c.is_whitespace()).count();
        if ind <= base {
            break;
        }
        end = i + 1;
    }
    end
}

/// ファイル本文から構造単位を抽出（Python は前行の decorator を単位に含める）
fn extract_units(file: &str, lang: &str, src: &str) -> Vec<Unit> {
    let lines: Vec<String> = src.lines().map(String::from).collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some((kind, name)) = unit_start(lang, l) else {
            continue;
        };
        // decorator（@xxx）を単位開始へ含める（Python）
        let mut start = i + 1;
        if lang == "python" {
            while start > 1 && lines[start - 2].trim_start().starts_with('@') {
                start -= 1;
            }
        }
        let end = if lang == "python" {
            indent_end(&lines, i + 1)
        } else {
            brace_end(&lines, i + 1)
        };
        out.push(Unit {
            file: file.to_string(),
            kind,
            name,
            start,
            end,
        });
    }
    out
}

// ---------- repo 走査・トークン照合 ----------

const SKIP_DIRS: &[&str] = &[
    ".git", "target", "node_modules", "dist", "build", ".next", "vendor",
    "__pycache__", ".venv", "venv", "out", "coverage",
];

fn codemap_cfg<'a>(cfg: &'a Value, key: &str) -> Option<&'a Value> {
    cfg_obj(cfg, "codeMap").and_then(|m| m.get(key))
}

fn cfg_enabled(cfg: &Value) -> bool {
    codemap_cfg(cfg, "enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn cfg_num(cfg: &Value, key: &str, default: f64) -> usize {
    codemap_cfg(cfg, key)
        .and_then(|v| v.as_f64())
        .unwrap_or(default) as usize
}

/// 課題 title+body から照合トークンを抽出する。識別子・パス片の
/// 3 文字以上の連続英数（_を含む）を取り、パス区切りの構成要素も
/// 個別トークンとして畳み込む（上限 40 個）
fn tokens_of(issue: &Value) -> Vec<String> {
    let text = format!(
        "{}\n{}",
        issue.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        issue.get("body").and_then(|v| v.as_str()).unwrap_or("")
    );
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    // パス・ドット区切りも含む粗い語を先に採り、構成要素に分解して登録する
    for raw in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '/' || c == '.' || c == '-')) {
        let raw = raw.trim_matches(|c| c == '.' || c == '/' || c == '-');
        if raw.len() < 3 {
            continue;
        }
        for part in raw.split(|c| c == '/' || c == '.') {
            let p = part.trim_matches('-');
            if p.len() < 3 || !p.chars().any(|c| c.is_ascii_alphabetic()) {
                continue;
            }
            // 拡張子らしい語・汎用語は照合価値が低いので落とす
            if matches!(
                p.to_ascii_lowercase().as_str(),
                "the" | "and" | "for" | "http" | "https" | "com" | "www"
                    | "md" | "json" | "html" | "png" | "jpg"
            ) {
                continue;
            }
            let l = p.to_ascii_lowercase();
            if seen.insert(l.clone()) {
                out.push(l);
                if out.len() >= 40 {
                    return out;
                }
            }
        }
    }
    out
}

/// 単位の関連スコア（codemap.mmd）: 単位名一致 +3 / パス片一致 +2 /
/// ファイル stem 一致 +1。トークンごとの最高寄与を合算する
fn score_unit(u: &Unit, tokens: &[String]) -> i64 {
    let name = u.name.to_ascii_lowercase();
    let path = u.file.to_ascii_lowercase();
    let stem = Path::new(&u.file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut score = 0i64;
    for t in tokens {
        if name == *t {
            score += 3;
        } else if name.contains(t.as_str()) {
            score += 2;
        } else if path.split('/').any(|seg| seg == *t) {
            score += 2;
        } else if stem == *t {
            score += 1;
        }
    }
    score
}

/// repo を走査して構造単位を収集する。maxFiles・maxFileBytes で打切る
fn scan_units(repo: &Path, cfg: &Value) -> Vec<Unit> {
    let max_files = cfg_num(cfg, "maxFiles", 2000.0);
    let max_file_bytes = cfg_num(cfg, "maxFileBytes", 200_000.0);
    let mut units = Vec::new();
    let mut stack = vec![repo.to_path_buf()];
    let mut seen_files = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let skip = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| SKIP_DIRS.contains(&n) || n.starts_with('.'))
                    .unwrap_or(true);
                if !skip {
                    stack.push(p);
                }
                continue;
            }
            let Some(lang) = lang_of(&p) else {
                continue;
            };
            let rel = p
                .strip_prefix(repo)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let Ok(meta) = p.metadata() else {
                continue;
            };
            if meta.len() as usize > max_file_bytes {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&p) else {
                continue;
            };
            seen_files += 1;
            units.extend(extract_units(&rel, lang, &src));
            if seen_files >= max_files {
                return units;
            }
        }
    }
    units
}

/// dispatch prompt へ注入する「関連コード構造（AST 単位）」節を組み立てる。
/// ヒット 0 件・無効化・repo 不在は None（呼出側は節を付けない）
pub fn context_section(repo: &Path, issue: &Value, cfg: &Value) -> Option<String> {
    if !cfg_enabled(cfg) || !repo.is_dir() {
        return None;
    }
    let tokens = tokens_of(issue);
    if tokens.is_empty() {
        return None;
    }
    let max_units = cfg_num(cfg, "maxUnits", 12.0);
    let max_bytes = cfg_num(cfg, "maxBytes", 9000.0);
    let mut units = scan_units(repo, cfg);
    units.retain(|u| score_unit(u, &tokens) > 0);
    if units.is_empty() {
        return None;
    }
    units.sort_by(|a, b| {
        score_unit(b, &tokens)
            .cmp(&score_unit(a, &tokens))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start.cmp(&b.start))
    });
    units.truncate(max_units);
    let mut out = String::from("## 関連コード構造（AST 単位）\n\n");
    for u in &units {
        let head = format!("### {}:{}-{} {} {}\n", u.file, u.start, u.end, u.kind, u.name);
        let lang_tag = match lang_of(repo.join(&u.file).as_path()).unwrap_or("") {
            "rust" => "rust",
            "python" => "python",
            _ => "",
        };
        // 単位スニペット（長大な impl 等は先頭 60 行で打切り）
        let snippet = std::fs::read_to_string(repo.join(&u.file))
            .map(|s| {
                s.lines()
                    .skip(u.start - 1)
                    .take(u.end - u.start + 1)
                    .take(60)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let block = format!("{}```{}
{}
```\n\n", head, lang_tag, snippet);
        if out.len() + block.len() > max_bytes {
            break;
        }
        out.push_str(&block);
    }
    Some(out)
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RS: &str = r#"// comment
use std::io;

pub struct Config {
    pub name: String,
}

impl Config {
    pub fn load(&self) -> bool {
        true
    }
}

fn helper() {
    let s = "{ not a brace }";
    println!("{}", s);
}

mod inner;
"#;

    #[test]
    fn rust_units_have_ranges() {
        let us = extract_units("src/a.rs", "rust", RS);
        let names: Vec<(&str, &str)> = us.iter().map(|u| (u.kind.as_str(), u.name.as_str())).collect();
        assert!(names.contains(&("struct", "Config")));
        assert!(names.contains(&("impl", "Config")));
        assert!(names.contains(&("fn", "helper")));
        assert!(names.contains(&("mod", "inner")));
        // impl ブロックは helper までを含む範囲
        let imp = us.iter().find(|u| u.kind == "impl").unwrap();
        assert_eq!(imp.start, 8);
        assert_eq!(imp.end, 12);
        // 文字列中のブレースを誤カウントしない
        let h = us.iter().find(|u| u.name == "helper").unwrap();
        assert_eq!(h.end, 17);
    }

    const PY: &str = r#"import os

@decorator
def fetch_data(url):
    x = "{"
    return x

class Loader:
    def run(self):
        pass

def tail():
    pass
"#;

    #[test]
    fn python_units_use_indent_and_decorators() {
        let us = extract_units("tool/b.py", "python", PY);
        let d = us.iter().find(|u| u.name == "fetch_data").unwrap();
        assert_eq!(d.start, 3); // @decorator 行を含む
        assert_eq!(d.end, 6);
        let l = us.iter().find(|u| u.name == "Loader").unwrap();
        assert_eq!(l.start, 8);
        assert_eq!(l.end, 10);
        let t = us.iter().find(|u| u.name == "tail").unwrap();
        assert_eq!(t.end, 13);
    }

    const JS: &str = r#"import x from 'y';

export function renderApp() {
  return `<div>${'{'}</div>`;
}

const fetchUser = async (id) => {
  return id;
};

class Session {
}
"#;

    #[test]
    fn js_units_cover_function_class_arrow() {
        let us = extract_units("web/app.ts", "js", JS);
        let names: Vec<(&str, &str)> = us.iter().map(|u| (u.kind.as_str(), u.name.as_str())).collect();
        assert!(names.contains(&("function", "renderApp")));
        assert!(names.contains(&("arrow", "fetchUser")));
        assert!(names.contains(&("class", "Session")));
        let f = us.iter().find(|u| u.name == "renderApp").unwrap();
        assert_eq!(f.end, 5);
    }

    #[test]
    fn context_section_scores_and_caps() {
        let dir = std::env::temp_dir().join(format!(
            "kasugai-codemap-test-{}",
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/login.rs"),
            "pub fn login() {\n    auth();\n}\n\npub fn logout() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/unrelated.rs"),
            "pub fn nothing() {}\n",
        )
        .unwrap();
        let issue = json!({"title": "login が失敗する", "body": "login の不具合を修正する"});
        let sec = context_section(&dir, &issue, &json!({})).unwrap();
        assert!(sec.contains("login.rs"));
        assert!(sec.contains("fn login"));
        assert!(!sec.contains("nothing"));
        // ヒット無しは None
        let miss = context_section(&dir, &json!({"title":"zzz","body":"qqq"}), &json!({}));
        assert!(miss.is_none());
        // 無効化は None
        let off = context_section(&dir, &issue, &json!({"codeMap": {"enabled": false}}));
        assert!(off.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

