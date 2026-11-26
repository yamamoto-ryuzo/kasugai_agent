// heartbeat.rs — デッドマンスイッチ heartbeat（heartbeat.mmd・README §11）
// agent 自身の死を外部へ通知する唯一の経路: 稼働中は secrets.json の
// heartbeatUrl へ config.heartbeatMinutes 間隔で単純 GET ping を送る。
// ping 途絶 = agent 全停止として外部サービス側が通知する
// （healthchecks.io / Dead Man's Snitch 互換の単純 GET）。
// 送信先 URL は secrets.json のみに置き、config.json・ログには書かない。
// 未設定・heartbeatMinutes<=0 なら無効のまま（外部サービス登録は人が行う）。
// runner/verify/drain の監視対象パイプラインとは別経路の独立タスク。
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::conlog::conlog;
use crate::config::{cfg_f64, load_j, secrets_path};
use crate::control::App;
use crate::util::now_iso;

const PING_TIMEOUT: Duration = Duration::from_secs(15);
// 無効時の設定再読込間隔（secrets/config はループごとに読み直す）
const RECHECK: Duration = Duration::from_secs(60);

// 最終 ping の稼働状態（/api/state.heartbeat。URL 自体は secrets のため出さない）
static STATUS: LazyLock<Mutex<Value>> =
    LazyLock::new(|| Mutex::new(json!({"enabled": false})));

pub fn status() -> Value {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_status(v: Value) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = v;
}

/// 送信先の解決。(url, minutes)。未設定・間隔<=0 なら None（無効のまま）
fn target(root: &Path, cfg: &Value) -> Option<(String, f64)> {
    let mins = cfg_f64(cfg, "heartbeatMinutes", 5.0);
    if mins <= 0.0 {
        return None;
    }
    let url = load_j(&secrets_path(root), json!({}))
        .get("heartbeatUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if url.is_empty() {
        return None;
    }
    Some((url, mins))
}

/// control 起動時に spawn する定期 ping ループ（heartbeat.mmd）。
/// ping 成否の変化時のみ SSE push する（成功ごとの push はしない）。
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut was_on = false;
        let mut last_ok: Option<bool> = None;
        loop {
            let cfg = app.cfg().await;
            let Some((url, mins)) = target(&app.root, &cfg) else {
                set_status(json!({"enabled": false}));
                if was_on {
                    app.sse("heartbeat", &status());
                }
                was_on = false;
                last_ok = None;
                tokio::time::sleep(RECHECK).await;
                continue;
            };
            was_on = true;
            let ping = app
                .http
                .get(&url)
                .timeout(PING_TIMEOUT)
                .send()
                .await;
            let (ok, errm) = match &ping {
                Ok(r) => (r.status().is_success(), String::new()),
                Err(e) => (false, e.to_string()),
            };
            if !errm.is_empty() {
                conlog(
                    "heartbeat",
                    "err",
                    &format!("heartbeat ping 失敗: {}", errm),
                );
            }
            let mut st = json!({
                "enabled": true,
                "minutes": mins,
                "lastPingAt": now_iso(),
                "lastPingOk": ok,
            });
            if !errm.is_empty() {
                st["lastPingError"] = json!(errm);
            }
            set_status(st);
            if last_ok != Some(ok) {
                app.sse("heartbeat", &status());
            }
            last_ok = Some(ok);
            tokio::time::sleep(Duration::from_secs_f64(mins * 60.0)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-heartbeat-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn target_disabled_without_url_or_interval() {
        let root = temp_root("off");
        // secrets 未設定 → 無効
        assert!(target(&root, &json!({})).is_none());
        // URL ありでも heartbeatMinutes<=0 → 無効
        crate::config::save_j(
            &root.join("secrets.json"),
            &json!({"heartbeatUrl": "https://example.com/ping"}),
        );
        assert!(target(&root, &json!({"heartbeatMinutes": 0})).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn target_resolves_url_and_interval() {
        let root = temp_root("on");
        crate::config::save_j(
            &root.join("secrets.json"),
            &json!({"heartbeatUrl": "https://example.com/ping"}),
        );
        // 既定 5 分
        assert_eq!(
            target(&root, &json!({})),
            Some(("https://example.com/ping".into(), 5.0))
        );
        assert_eq!(
            target(&root, &json!({"heartbeatMinutes": 10})),
            Some(("https://example.com/ping".into(), 10.0))
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
