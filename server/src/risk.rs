// risk.rs — action レベルのリスク分類（run-control.mmd・manual-cf669d09）
//
// OpenHands の Confirmation Policy×Security Analyzer（ConfirmRisky）を参考に、
// 実行体の出力行に現れる個別アクションをリスク分類する。verifyLevel とは
// 別軸のポリシーで、HIGH は人確認（run halt → riskHold → risk-ack の
// 人判断。拒否時は理由を次回 prompt に注入して別アプローチを促す）。
// -p 非対話実行は tool call を構造取得できないため、コマンド実行を匂わせる
// 行を静的パターンで分類する（誤検は人が risk-ack で解除する）。
//
//   HIGH（既定・人確認必須）:
//     破壊的削除（rm -rf / Remove-Item -Recurse / del・rd /S 等）
//     git push --force / reset --hard / clean -f / checkout -- / restore /
//     実行体自身による git push（検証未通の公開は verify の責務）
//     設定変更（reg add/delete・HKLM|HKCU 書込み・secrets.json への書込み）
//     外部への状態変更（npm|cargo publish・gh issue|pr create・破壊的SQL）
//   MEDIUM（記録のみ・halt しない）:
//     外部通信（curl/wget/Invoke-WebRequest 等）・依存追加
//     （npm install / pip install / cargo add 等）
//
// config.riskHighPatterns に正規表現の文字列を足すと HIGH パターンを
// 追加できる（誤検の緩和ではなく検出強化用。無効化は riskConfirm=off）
use regex::Regex;
use serde_json::Value;

pub struct Risk {
    pub level: &'static str, // "HIGH" | "MEDIUM"
    pub label: &'static str,
    pub line: String,
}

const HIGH_PATTERNS: &[(&str, &str)] = &[
    (r"(?i)\brm\s+(-[a-z]*[rf][a-z]*\s+)+\S", "recursive-delete"),
    (r"(?i)\b(remove-item|ri)\b[^\n]*-(recurse|force)", "recursive-delete"),
    (r"(?i)\b(rd|rmdir|del|erase)\s+[^&|\n]*(/s|/f|/q)", "recursive-delete"),
    (r"(?i)\bgit\s+push\b[^&|\n]*(-f\b|--force\b)", "git-force-push"),
    (r"(?i)\bgit\s+push\b", "git-push"),
    (r"(?i)\bgit\s+reset\s+--hard\b", "git-destructive"),
    (r"(?i)\bgit\s+clean\b[^&|\n]*-[a-z]*f", "git-destructive"),
    (r"(?i)\bgit\s+(checkout|restore)\b[^&|\n]*--", "git-destructive"),
    (r"(?i)\breg\s+(add|delete)\b", "config-write"),
    (r"(?i)(HKLM|HKCU)[^\n]*(set-itemproperty|new-item|reg\s+add)", "config-write"),
    (r"(?i)secrets\.json[^\n]*(write|>|>>|out-file|set-content)", "secrets-write"),
    (r"(?i)\bnpm\s+publish\b|\bcargo\s+publish\b|\bgem\s+push\b", "external-publish"),
    (r"(?i)\bgh\s+(issue|pr)\s+(create|edit|close|merge)\b", "external-write"),
    (r"(?i)\b(drop\s+table|drop\s+database|delete\s+from|truncate\s+table)\b", "data-destructive"),
];

const MEDIUM_PATTERNS: &[(&str, &str)] = &[
    (r"(?i)\b(curl|wget|invoke-webrequest|invoke-restmethod|iwr|irm)\b", "external-network"),
    (r"(?i)\b(npm|pnpm|yarn)\s+(install|i|add)\b|\bpip\s+install\b|\bcargo\s+add\b", "dependency-add"),
];

/// config.riskHighPatterns（文字列の正規表現配列）を取り出す
fn extra_high(cfg: &Value) -> Vec<String> {
    cfg.get("riskHighPatterns")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn compile(
    patterns: &[(&'static str, &'static str)],
    extra: &[String],
) -> Vec<(Regex, &'static str)> {
    let mut v: Vec<(Regex, &'static str)> = patterns
        .iter()
        .filter_map(|(p, l)| Regex::new(p).ok().map(|r| (r, *l)))
        .collect();
    for p in extra {
        if let Ok(r) = Regex::new(p) {
            v.push((r, "config-high"));
        }
    }
    v
}

/// run 1回分のスキャナ（パターンは起動時に1回コンパイル）
pub struct Scanner {
    high: Vec<(Regex, &'static str)>,
    medium: Vec<(Regex, &'static str)>,
}

impl Scanner {
    pub fn new(cfg: &Value) -> Self {
        Self {
            high: compile(HIGH_PATTERNS, &extra_high(cfg)),
            medium: compile(MEDIUM_PATTERNS, &[]),
        }
    }

    /// 行をリスク分類する。HIGH → MEDIUM の順で最初に合った分類を返す。
    /// 検査対象は実行体の出力行そのもの（コマンドエコー・ログ行を含む）
    pub fn classify(&self, line: &str) -> Option<Risk> {
        for (re, label) in &self.high {
            if re.is_match(line) {
                return Some(Risk {
                    level: "HIGH",
                    label,
                    line: line.chars().take(300).collect(),
                });
            }
        }
        for (re, label) in &self.medium {
            if re.is_match(line) {
                return Some(Risk {
                    level: "MEDIUM",
                    label,
                    line: line.chars().take(300).collect(),
                });
            }
        }
        None
    }
}

/// 単発分類（テスト・一括評価用。毎回コンパイルするので監視ループには
/// Scanner を使うこと）
pub fn classify(cfg: &Value, line: &str) -> Option<Risk> {
    Scanner::new(cfg).classify(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn high_on_destructive_and_push() {
        let cfg = json!({});
        assert_eq!(
            classify(&cfg, "$ rm -rf ./dist").unwrap().label,
            "recursive-delete"
        );
        assert_eq!(
            classify(&cfg, "git push --force origin main").unwrap().label,
            "git-force-push"
        );
        assert_eq!(
            classify(&cfg, "git push origin main").unwrap().label,
            "git-push"
        );
        assert_eq!(
            classify(&cfg, "reg add HKCU\\Software\\X /v Y").unwrap().label,
            "config-write"
        );
        assert_eq!(
            classify(&cfg, "npm publish --access public").unwrap().label,
            "external-publish"
        );
    }

    #[test]
    fn medium_on_network_and_deps() {
        let cfg = json!({});
        assert_eq!(
            classify(&cfg, "curl https://example.com/x.zip -o x.zip")
                .unwrap()
                .level,
            "MEDIUM"
        );
        assert_eq!(
            classify(&cfg, "npm install lodash").unwrap().level,
            "MEDIUM"
        );
    }

    #[test]
    fn no_hit_on_normal_output() {
        let cfg = json!({});
        assert!(classify(&cfg, "cargo test → 12 passed").is_none());
        assert!(classify(&cfg, "write file src/lib.rs").is_none());
    }

    #[test]
    fn config_extra_high_pattern() {
        let cfg = json!({"riskHighPatterns": ["dangerous-cmd"]});
        assert_eq!(
            classify(&cfg, "run dangerous-cmd now").unwrap().level,
            "HIGH"
        );
    }
}
