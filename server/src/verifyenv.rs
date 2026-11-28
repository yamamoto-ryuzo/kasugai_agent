//! repo 別 verify 環境の宣言的セットアップ記述（manual-04c35958）。
//! 各 repo ルートの `verify-env.toml` を読み、run/verify が参照する
//! （copilot-setup-steps.yml / Devin environment snapshot 相当を
//! repo 内ファイルで宣言する。正本は verify-levels.mmd の VENV ノード）。
//!
//! スキーマ:
//!   setup = [...]   verify 前に一度だけ順に実行するセットアップコマンド
//!   checks = [...]  L1 チェック。非空なら自動検出を上書き
//!   [env]           全コマンドへ注入する環境変数
//!   [probe]         L2+ の実機起動定義（exe/args/port/timeoutSecs）
//!
//! TOML のため `setup`/`checks` はテーブル宣言（[env]・[probe]）より前の
//! トップレベルに書く（テーブル行より後に書くとそのテーブル配下になる）。
//!
//! コマンド文字列は空白で argv 分割する（クオート記法は非対応）。

use serde_json::Value;
use std::path::Path;

/// verify-env.toml を読み Value として返す（不在・パース失敗は None）
pub fn load(repo_path: &Path) -> Option<Value> {
    let s = std::fs::read_to_string(repo_path.join("verify-env.toml")).ok()?;
    let t: toml::Value = s.parse().ok()?;
    serde_json::to_value(t).ok()
}

/// [env] セクションの環境変数（全コマンドへ注入）
pub fn env_vars(env: &Value) -> Vec<(String, String)> {
    env.get("env")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// setup / checks の文字列コマンドを argv へ（空白分割。クオート不可）
fn cmds(env: &Value, key: &str) -> Vec<Vec<String>> {
    env.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str())
                .map(|s| {
                    s.split_whitespace()
                        .map(String::from)
                        .collect::<Vec<String>>()
                })
                .filter(|v| !v.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// verify 前に一度だけ順に実行するセットアップコマンド（失敗で verify NG）
pub fn setup(env: &Value) -> Vec<Vec<String>> {
    cmds(env, "setup")
}

/// L1 チェック。非空なら detect_checks の自動検出を上書きする
pub fn checks(env: &Value) -> Vec<Vec<String>> {
    cmds(env, "checks")
}

/// [probe] セクション（L2+ の実機起動定義）
pub fn probe(env: &Value) -> Option<&Value> {
    env.get("probe")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(t: &str) -> Value {
        serde_json::to_value(t.parse::<toml::Value>().unwrap()).unwrap()
    }

    #[test]
    fn parses_full_schema() {
        let v = parse(
            r#"
setup = ["cargo fetch", "npm ci"]
checks = ["cargo check", "cargo test -- --nocapture"]

[env]
A = "1"
B = "two"

[probe]
exe = "target/debug/app.exe"
args = ["--serve", "--port", "9999"]
port = 9999
timeoutSecs = 15
"#,
        );
        let env = env_vars(&v);
        assert_eq!(env.len(), 2);
        assert!(env.contains(&("A".into(), "1".into())));
        assert!(env.contains(&("B".into(), "two".into())));
        assert_eq!(
            setup(&v),
            vec![vec!["cargo".to_string(), "fetch".to_string()], vec!["npm".to_string(), "ci".to_string()]]
        );
        assert_eq!(checks(&v).len(), 2);
        let p = probe(&v).unwrap();
        assert_eq!(
            p.get("exe").and_then(|x| x.as_str()),
            Some("target/debug/app.exe")
        );
        assert_eq!(p.get("port").and_then(|x| x.as_u64()), Some(9999));
        assert_eq!(p.get("timeoutSecs").and_then(|x| x.as_u64()), Some(15));
    }

    #[test]
    fn absent_or_partial_is_empty() {
        assert!(load(Path::new("Z:/definitely/missing-dir")).is_none());
        let v = parse("");
        assert!(env_vars(&v).is_empty());
        assert!(setup(&v).is_empty());
        assert!(checks(&v).is_empty());
        assert!(probe(&v).is_none());
    }
}
