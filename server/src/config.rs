// config.rs — config.json / config.example.json / secrets.json / chats.json 等の
// JSON 永続化（control.mjs の loadJ/saveJ 相当）。
// config は浅いマージ（example ← config）で Value として保持し、
// persist は config.json の他キーを壊さないよう「読み直して差分書込み」する。
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn load_j(p: &Path, default: Value) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

pub fn save_j(p: &Path, v: &Value) {
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = std::fs::write(p, s);
    }
}

/// config.example.json ← config.json の浅いマージ
pub fn load_config(root: &Path) -> Value {
    let mut base = load_j(&root.join("config.example.json"), json!({}));
    let user = load_j(&root.join("config.json"), json!({}));
    if let (Some(b), Some(u)) = (base.as_object_mut(), user.as_object()) {
        for (k, v) in u {
            b.insert(k.clone(), v.clone());
        }
        return base;
    }
    user
}

/// persistConfig: config.json を読み直し、repos/chatAgents/chatAgent/
/// issueExecMode だけ差し替えて保存
pub fn persist_config(root: &Path, cfg: &Value) {
    let path = root.join("config.json");
    let mut cur = load_j(&path, json!({}));
    if let (Some(c), Some(live)) = (cur.as_object_mut(), cfg.as_object()) {
        for k in [
            "repos",
            "chatAgents",
            "chatAgent",
            "issueExecMode",
            "maxConcurrentSessions",
            // 並列モード切替（set_parallel_exec）。未登録だと再起動で
            // 直列ガードへ巻き戻る
            "conflictWait",
        ] {
            match live.get(k) {
                Some(v) => {
                    c.insert(k.to_string(), v.clone());
                }
                None => {
                    c.remove(k);
                }
            }
        }
    }
    save_j(&path, &cur);
}

// ---- アクセサ ----
pub fn cfg_u64(cfg: &Value, key: &str, default: u64) -> u64 {
    cfg.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}
pub fn cfg_f64(cfg: &Value, key: &str, default: f64) -> f64 {
    cfg.get(key).and_then(|v| v.as_f64()).unwrap_or(default)
}
pub fn cfg_str<'a>(cfg: &'a Value, key: &str) -> Option<&'a str> {
    cfg.get(key).and_then(|v| v.as_str())
}
pub fn cfg_bool_default_true(cfg: &Value, key: &str) -> bool {
    cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(true)
}
pub fn cfg_bool_default_false(cfg: &Value, key: &str) -> bool {
    cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}
pub fn cfg_arr<'a>(cfg: &'a Value, key: &str) -> Vec<&'a Value> {
    cfg.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}
pub fn cfg_obj<'a>(cfg: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    cfg.get(key).and_then(|v| v.as_object())
}

// ---- secrets.json ----
pub fn secrets_path(root: &Path) -> PathBuf {
    root.join("secrets.json")
}

/// agent 定義の env マップを解決し (NAME, value) のペアで返す。
/// 値は `"リテラル"` / `"env:VAR"`（親環境から転写）/ `"secret:key"`
/// （secrets.json `tokens[key]` から解決）を取れる。
/// acp spawn・直接実行 runner の双方が使う共通解決。
pub fn agent_env(root: &Path, agent: &Value) -> Vec<(String, String)> {
    agent
        .get("env")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let s = v.as_str().unwrap_or("");
                    let val = if let Some(k2) = s.strip_prefix("secret:") {
                        secrets_get(root, k2)
                    } else if let Some(v2) = s.strip_prefix("env:") {
                        std::env::var(v2).ok()
                    } else {
                        Some(s.to_string())
                    };
                    val.map(|r| (k.clone(), r))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// secrets.json の tokens[<key>] を直接参照（key 指定の汎用ルート）
pub fn secrets_get(root: &Path, key: &str) -> Option<String> {
    load_j(&secrets_path(root), json!({}))
        .get("tokens")
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// secrets.json の tokens[<tokenKey ?? id>] を参照
pub fn secrets_token(root: &Path, agent: &Value, id: &str) -> Option<String> {
    let s = load_j(&secrets_path(root), json!({}));
    let key = agent
        .get("tokenKey")
        .and_then(|v| v.as_str())
        .filter(|k| !k.is_empty())
        .unwrap_or(id);
    s.get("tokens")
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// token 書込み（空文字/null で削除）
pub fn secrets_set_token(root: &Path, key: &str, value: Option<&str>) {
    let path = secrets_path(root);
    let mut s = load_j(&path, json!({}));
    let obj = s.as_object_mut().unwrap();
    if !obj.contains_key("tokens") {
        obj.insert("tokens".into(), json!({}));
    }
    let tokens = obj.get_mut("tokens").unwrap().as_object_mut().unwrap();
    match value {
        None | Some("") => {
            tokens.remove(key);
        }
        Some(v) => {
            tokens.insert(key.to_string(), Value::String(v.to_string()));
        }
    }
    save_j(&path, &s);
}
