// detector_health.rs — detector 沈黙検知 meta 監視台帳
// （detector-health.mmd・README §11）
// 「検出0件」自体が異常になり得るため、detector ごとの最終実行時刻
// （lastRunAt）と最終 emit 時刻（lastEmitAt）を detector-health.json に
// 記録する。detectorSilentMinutes（既定30・<=0 で無効）を超えて emit の
// ない detector は source=local-health の meta candidate として emit し、
// 通常の intake 経路で起票される。
// lastRunAt ≠ lastEmitAt の区別で「検出0件・正常（実行は継続）」と
// 「detector 停止・異常（poller 死亡/クラッシュ）」を分ける。
// 台帳は Git 管理外の稼働状態ファイル（.gitignore 登録済）。
// 複数 poller が同時に台帳を更新するため、LEDGER（メモリ上の正）を
// Mutex で直列化し、永続化も同じロック内で行う（load+save の
// read-modify-write 競合・書き込み途中ファイルの生成を防ぐ）。
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::config::{cfg_f64, load_j};
use crate::util::{now_iso, now_ms};

/// spawn_detector_poller が回す detector kind（pollSeconds のキー）
pub const KINDS: [&str; 4] = ["issues", "ciFailure", "specDrift", "health"];

const DEFAULT_SILENT_MINUTES: f64 = 30.0;

fn ledger_path(root: &Path) -> PathBuf {
    root.join("detector-health.json")
}

/// 台帳の正はメモリ上（初回アクセス時にディスクから seed）。
/// None = 未ロード
static LEDGER: LazyLock<Mutex<Option<Value>>> = LazyLock::new(|| Mutex::new(None));

/// 台帳への直列化アクセス。guard を保持したまま f を実行するため、
/// 読み取り・更新・永続化の全経路が1ロックに集約される
fn with_ledger<R>(root: &Path, f: impl FnOnce(&mut Value) -> R) -> R {
    let mut g = LEDGER.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(load_j(&ledger_path(root), json!({})));
    }
    let l = g.as_mut().unwrap();
    if !l.is_object() {
        *l = json!({});
    }
    f(l)
}

/// tmp 経由の置換で書き出し（クラッシュ時の中途半端なファイルを防ぐ）
fn save(root: &Path, v: &Value) {
    let p = ledger_path(root);
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// poller 1周期の記録。emitted = その poll が emit した candidate 数
pub fn record_run(root: &Path, kind: &str, emitted: usize) {
    let iso = now_iso();
    let ms = now_ms();
    with_ledger(root, |l| {
        let e = l
            .as_object_mut()
            .unwrap()
            .entry(kind.to_string())
            .or_insert_with(|| json!({}));
        if e.get("firstSeenAt").is_none() {
            e["firstSeenAt"] = json!(iso);
            e["firstSeenMs"] = json!(ms);
            e["runs"] = json!(0);
            e["emits"] = json!(0);
        }
        e["lastRunAt"] = json!(iso);
        e["lastRunMs"] = json!(ms);
        e["runs"] = json!(e["runs"].as_i64().unwrap_or(0) + 1);
        if emitted > 0 {
            e["lastEmitAt"] = json!(iso);
            e["lastEmitMs"] = json!(ms);
            e["emits"] = json!(e["emits"].as_i64().unwrap_or(0) + emitted as i64);
        }
        save(root, l);
    });
}

/// entry の状態: "ok" / "stopped"（実行停滞）/ "silent"（実行継続・emit 停滞）
/// 台帳未作成（初回 poll 前）は "ok" 扱い
fn state_of(e: &Value, silent_ms: f64, now: i64) -> &'static str {
    let run_ms = e.get("lastRunMs").and_then(|v| v.as_i64());
    let Some(run_ms) = run_ms else { return "ok" };
    if (now - run_ms) as f64 > silent_ms {
        return "stopped";
    }
    let emit_ms = e
        .get("lastEmitMs")
        .and_then(|v| v.as_i64())
        .or_else(|| e.get("firstSeenMs").and_then(|v| v.as_i64()));
    if let Some(ms) = emit_ms {
        if (now - ms) as f64 > silent_ms {
            return "silent";
        }
    }
    "ok"
}

/// 沈黙/停止している detector の meta candidate を返す
/// （detectorSilentMinutes<=0 なら検査しない）
pub fn check_silent(root: &Path, cfg: &Value) -> Vec<Value> {
    let mins = cfg_f64(cfg, "detectorSilentMinutes", DEFAULT_SILENT_MINUTES);
    if mins <= 0.0 {
        return Vec::new();
    }
    let silent_ms = mins * 60_000.0;
    let now = now_ms();
    with_ledger(root, |l| {
        let mut out = Vec::new();
        for kind in KINDS {
            let Some(e) = l.get(kind) else { continue };
            let (state, title, body) = match state_of(e, silent_ms, now) {
                "stopped" => (
                    "detector-stopped",
                    format!("detector 停止: {}", kind),
                    format!(
                        "{} detector の実行が {:.0} 分止まっています（最終実行 {}）。poller のクラッシュ・panic が疑われます。稼働状態の台帳は detector-health.json。",
                        kind,
                        (now - e["lastRunMs"].as_i64().unwrap_or(now)) as f64 / 60_000.0,
                        e["lastRunAt"].as_str().unwrap_or(""),
                    ),
                ),
                "silent" => {
                    let emit_ms = e
                        .get("lastEmitMs")
                        .and_then(|v| v.as_i64())
                        .or_else(|| e["firstSeenMs"].as_i64());
                    (
                        "detector-silent",
                        format!("detector 沈黙: {}", kind),
                        format!(
                            "{} detector は実行を続けていますが {:.0} 分 emit がありません（最終 emit {}）。認証切れ・対象消失・検出漏れの可能性。検出0件が継続する正常な沈黙なら却下してください。",
                            kind,
                            (now - emit_ms.unwrap_or(now)) as f64 / 60_000.0,
                            e.get("lastEmitAt")
                                .and_then(|v| v.as_str())
                                .unwrap_or("（まだ emit していない）"),
                        ),
                    )
                }
                _ => continue,
            };
            out.push(json!({
                "source": "local-health",
                "repo": "",
                "title": title,
                "body": body,
                "labels": ["health"],
                "kind": state,
                "symptomHash": format!("{}-{}", state, kind),
                "detectedAt": now_iso(),
            }));
        }
        out
    })
}

/// /api/state.detectorHealth: 台帳 + 各 detector の ok/silent/stopped
pub fn status(root: &Path, cfg: &Value) -> Value {
    let mins = cfg_f64(cfg, "detectorSilentMinutes", DEFAULT_SILENT_MINUTES);
    let now = now_ms();
    let silent_ms = mins * 60_000.0;
    with_ledger(root, |l| {
        let mut kinds = serde_json::Map::new();
        for k in KINDS {
            let mut e = l.get(k).cloned().unwrap_or_else(|| json!({}));
            e["state"] = json!(if mins <= 0.0 { "ok" } else { state_of(&e, silent_ms, now) });
            e.as_object_mut().map(|m| {
                m.remove("lastRunMs");
                m.remove("lastEmitMs");
                m.remove("firstSeenMs");
            });
            kinds.insert(k.to_string(), e);
        }
        json!({
            "silentMinutes": mins,
            "enabled": mins > 0.0,
            "kinds": kinds,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_when_last_run_stale() {
        let e = json!({"lastRunMs": 1000, "firstSeenMs": 1000});
        assert_eq!(state_of(&e, 60_000.0, 1000 + 61_000), "stopped");
    }

    #[test]
    fn silent_when_run_fresh_but_emit_stale() {
        // 実行は継続（lastRunMs 新しい）・emit は閾値超過 → silent
        let e = json!({"lastRunMs": 2000, "lastEmitMs": 1000, "firstSeenMs": 1000});
        assert_eq!(state_of(&e, 60_000.0, 62_000), "silent");
    }

    #[test]
    fn never_emitted_uses_first_seen_baseline() {
        let e = json!({"lastRunMs": 2000, "firstSeenMs": 1000});
        assert_eq!(state_of(&e, 60_000.0, 62_000), "silent");
    }

    #[test]
    fn ok_within_threshold_and_without_entry() {
        let e = json!({"lastRunMs": 2000, "lastEmitMs": 1900, "firstSeenMs": 1000});
        assert_eq!(state_of(&e, 60_000.0, 30_000), "ok");
        assert_eq!(state_of(&json!({}), 60_000.0, 30_000), "ok");
    }

    #[test]
    fn concurrent_record_run_keeps_valid_ledger() {
        // 複数 poller からの同時 record_run でも台帳ファイルが破損せず
        // 全 kind の記録が保持されること（load+save 競合の再現テスト）
        let root = std::env::temp_dir().join(format!(
            "kasugai-dethealth-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::remove_file(ledger_path(&root)).ok();
        // テスト間で共有される静的台帳をこの root で初期化してから並列実行
        with_ledger(&root, |l| *l = json!({}));
        let mut handles = Vec::new();
        for kind in KINDS {
            let r = root.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    record_run(&r, kind, i % 3);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = std::fs::read_to_string(ledger_path(&root)).unwrap();
        let l: Value = serde_json::from_str(&s).expect("ledger must stay valid JSON");
        for kind in KINDS {
            assert_eq!(l[kind]["runs"].as_i64().unwrap_or(0), 20, "{}", kind);
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
