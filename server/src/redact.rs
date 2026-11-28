// redact.rs — 証跡・trajectory・ログへの secret redaction（manual-a3a60c51）
//
// proposals の vet（事後検査）とは別に、生成段階で secret をマスクする
// （Cursor の secret redaction・Copilot egress firewall の思想）。
// 適用点:
//   - conlog（実行体出力・control stdio のリングバッファ = ログ）
//   - runner が書く evidence/<slug>/run.log・verify.log・status.txt
//   - trajectory（runner の tstep/step/meta 呼出時に scrub_value でマスク）
//   - knowledge/<slug>.md（run 結果・証跡を写す trajectory）
// 検出は2系統:
//   1. 形式パターン（SECRET_PATTERNS。proposals::vet の no-secrets 検査と共用）
//   2. secrets.json の登録値との完全一致（refresh で読み込み。形式を問わず
//      ユーザーの実キーを潰せる）
// マスク語は [REDACTED]。key=value 形はキー名だけ残す（可読性のため）。
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// シークレット混入検出パターン（proposals.rs の vet 検査と同一一覧。
/// 保守的に落とす。検出（secret_hit）・マスク（scrub）の双方がこの一覧を
/// 正本として使う）
pub(crate) const SECRET_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-]{20,}",
    r"ghp_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AKIA[0-9A-Z]{16}",
    r"AIza[0-9A-Za-z_-]{35}",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    r#"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*["'][^"'\s]{12,}"#,
];

/// マスク用パターンと置換語の対。末尾の key=value 形だけは
/// $1（キー名）を残す。他は全体を [REDACTED] に畳む
const MASKS: &[(&str, &str)] = &[
    (r"sk-[A-Za-z0-9_-]{20,}", "[REDACTED]"),
    (r"ghp_[A-Za-z0-9]{20,}", "[REDACTED]"),
    (r"github_pat_[A-Za-z0-9_]{20,}", "[REDACTED]"),
    (r"xox[baprs]-[A-Za-z0-9-]{10,}", "[REDACTED]"),
    (r"AKIA[0-9A-Z]{16}", "[REDACTED]"),
    (r"AIza[0-9A-Za-z_-]{35}", "[REDACTED]"),
    (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "[REDACTED]"),
    (
        r#"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*["'][^"'\s]{12,}"#,
        "$1=<REDACTED>",
    ),
];

fn mask_res() -> &'static Vec<(Regex, &'static str)> {
    static RE: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RE.get_or_init(|| {
        MASKS.iter()
            .filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, *r)))
            .collect()
    })
}

/// secrets.json 由来の登録値（プロセス内キャッシュ。
/// main 起動時と drain 定期 tick が refresh する）
static VALUES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn values() -> &'static Mutex<Vec<String>> {
    VALUES.get_or_init(|| Mutex::new(Vec::new()))
}

/// secrets.json の全文字列リーフを登録値として読み込み直す。
/// 8 文字未満は誤爆防止で登録しない（短い一般語を潰さないため）。
/// tokens{} 以外の手書きキーも拾うため再帰走査する
pub fn refresh(root: &Path) {
    let s = crate::config::load_j(&crate::config::secrets_path(root), json!({}));
    let mut vals = Vec::new();
    collect_values(&s, &mut vals);
    // 長い値から先に潰す（部分包含する値があっても先に全体を消す）
    vals.sort_by_key(|b| std::cmp::Reverse(b.len()));
    *values().lock().unwrap() = vals;
}

fn collect_values(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.len() >= 8 => out.push(s.clone()),
        Value::Array(a) => {
            for x in a {
                collect_values(x, out);
            }
        }
        Value::Object(o) => {
            for x in o.values() {
                collect_values(x, out);
            }
        }
        _ => {}
    }
}

/// text 中の secret を [REDACTED] へ置換する。
/// 登録値（完全一致）→ 形式パターンの順で適用する
pub fn scrub(text: &str) -> String {
    let mut out = text.to_string();
    for v in values().lock().unwrap().iter() {
        if out.contains(v.as_str()) {
            out = out.replace(v.as_str(), "[REDACTED]");
        }
    }
    for (re, rep) in mask_res() {
        out = re.replace_all(&out, *rep).into_owned();
    }
    out
}

/// JSON 値の全文字列リーフへ scrub を再帰適用する
/// （trajectory の step data・meta 等、構造化証跡のマスク用）
pub fn scrub_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(scrub(s)),
        Value::Array(a) => Value::Array(a.iter().map(scrub_value).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), scrub_value(x)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

/// content にシークレットらしき文字列があれば理由を返す
/// （proposals::vet の no-secrets 検査が共有する検出面。置換しない）
pub fn secret_hit(content: &str) -> Option<String> {
    for pat in SECRET_PATTERNS {
        if let Ok(re) = Regex::new(pat) {
            if let Some(m) = re.find(content) {
                return Some(format!("pattern {} matched at {}", pat, m.start()));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-redact-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn scrub_masks_token_patterns() {
        let s = "fetch ghp_0123456789abcdef0123456789 done";
        let out = scrub(s);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("ghp_0123"));
        // sk- / private key も潰れる
        assert!(!scrub("k = sk-abcdefghijklmnopqrstuvwxyz123456").contains("sk-abcd"));
        assert!(!scrub("-----BEGIN RSA PRIVATE KEY-----\nMII").contains("PRIVATE KEY-----"));
    }

    #[test]
    fn scrub_keeps_key_name_for_kv_form() {
        let out = scrub("token: \"abcdef0123456789xyz\"");
        assert!(out.contains("token=<REDACTED>"), "got: {}", out);
        assert!(!out.contains("abcdef0123"));
    }

    #[test]
    fn scrub_masks_registered_secret_values() {
        let root = temp_root("vals");
        crate::config::save_j(
            &crate::config::secrets_path(&root),
            &json!({"tokens": {"main": "my-very-secret-token-value-1"}}),
        );
        refresh(&root);
        let out = scrub("Authorization: Bearer my-very-secret-token-value-1 ok");
        assert!(!out.contains("my-very-secret"), "got: {}", out);
        assert!(out.contains("[REDACTED]"));
        // 登録値クリア（他テストへの漏洩防止）
        *values().lock().unwrap() = vec![];
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scrub_value_masks_nested_strings() {
        let v = json!({
            "line": "call with ghp_0123456789abcdef0123456789",
            "nested": ["x", {"reason": "k = sk-abcdefghijklmnopqrstuvwxyz123456"}],
            "n": 3,
        });
        let out = scrub_value(&v);
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("ghp_0123"), "got: {}", s);
        assert!(!s.contains("sk-abcd"), "got: {}", s);
        assert!(s.contains("[REDACTED]"));
        assert_eq!(out.get("n").and_then(|x| x.as_u64()), Some(3));
    }

    #[test]
    fn secret_hit_detects_and_clean_passes() {
        assert!(secret_hit("x ghp_0123456789abcdef0123456789 y").is_some());
        assert!(secret_hit("ordinary text no secrets").is_none());
    }
}
