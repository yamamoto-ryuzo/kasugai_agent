// devenvs.rs — 開発環境の起動管理
// 正本: diagrams/dev-envs.mmd → README §8「開発環境の起動管理」
//   - config.devEnvironments[] = {id,label,cwd,command,port,url}
//   - GET /api/dev-envs / POST /api/dev-envs/launch / POST /api/dev-envs/stop
//     （control.rs）
//   - command は cmd /c（POSIX は sh -c）経由で実行するため cargo run /
//     npm run dev / python run.py 等をそのまま書ける
//   - 起動判定は port への TCP 応答のみ。停止は port 占有 PID の taskkill
//     （tools/dev.mjs と同じ解放手順）

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub struct DevEnv {
    pub id: String,
    pub label: String,
    pub cwd: PathBuf,
    pub command: String,
    pub port: u16,
    pub url: Option<String>,
    // 子プロセスへ追加する環境変数（config の env オブジェクト）
    pub env: Vec<(String, String)>,
}

impl DevEnv {
    pub fn url_or_default(&self) -> String {
        self.url
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}/", self.port))
    }
}

// config.devEnvironments のパース（必須: id/cwd/command。port=0 なら
// 状態判定なし・常に起動ボタンのみ）
pub fn envs(cfg: &Value) -> Vec<DevEnv> {
    crate::config::cfg_arr(cfg, "devEnvironments")
        .into_iter()
        .filter_map(|e| {
            Some(DevEnv {
                id: e.get("id")?.as_str()?.to_string(),
                label: e
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                cwd: PathBuf::from(e.get("cwd")?.as_str()?),
                command: e.get("command")?.as_str()?.to_string(),
                port: e.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
                url: e
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                env: e
                    .get("env")
                    .and_then(|v| v.as_object())
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| {
                                v.as_str().map(|s| (k.clone(), s.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

pub fn find(cfg: &Value, id: &str) -> Option<DevEnv> {
    envs(cfg).into_iter().find(|e| e.id == id)
}

fn log_path(root: &Path, id: &str) -> PathBuf {
    root.join("logs").join(format!("devenv-{}.log", id))
}

// ログ末尾（表示用。最大 8 行 / 4KB）
fn log_tail(root: &Path, id: &str) -> String {
    let p = log_path(root, id);
    let Ok(data) = std::fs::read(&p) else {
        return String::new();
    };
    let start = data.len().saturating_sub(4096);
    let s = String::from_utf8_lossy(&data[start..]);
    s.lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

// GET /api/dev-envs の1行分
pub async fn describe(root: &Path, e: &DevEnv) -> Value {
    let alive = e.port != 0 && crate::launch::port_alive(e.port).await;
    json!({
        "id": e.id,
        "label": if e.label.is_empty() { &e.id } else { &e.label },
        "cwd": e.cwd.to_string_lossy(),
        "command": e.command,
        "port": e.port,
        "url": e.url_or_default(),
        "alive": alive,
        "log": log_tail(root, &e.id),
    })
}

// ---------- port 占有プロセスの解放（dev.mjs と同じ手順） ----------

#[cfg(windows)]
fn pids_on_port(port: u16) -> Vec<u32> {
    let mut c = std::process::Command::new("netstat");
    c.args(["-ano", "-p", "tcp"]);
    crate::util::hide_console(&mut c);
    let Ok(out) = c.output() else {
        return vec![];
    };
    if !out.status.success() {
        return vec![];
    }
    let suffix = format!(":{}", port);
    let mut pids = vec![];
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // "  TCP    127.0.0.1:8586    0.0.0.0:0    LISTENING    12345"
        if cols.len() >= 5
            && cols[0] == "TCP"
            && cols[3].eq_ignore_ascii_case("LISTENING")
            && cols[1].ends_with(&suffix)
        {
            if let Ok(pid) = cols[4].parse::<u32>() {
                if pid != std::process::id() && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

#[cfg(not(windows))]
fn pids_on_port(_port: u16) -> Vec<u32> {
    vec![]
}

// port 占有プロセスを taskkill /F /T。殺した PID 一覧を返す
pub fn kill_port(port: u16) -> Vec<u32> {
    let pids = pids_on_port(port);
    for pid in &pids {
        #[cfg(windows)]
        {
            let mut c = std::process::Command::new("taskkill");
            c.args(["/F", "/T", "/PID", &pid.to_string()]);
            crate::util::hide_console(&mut c);
            let _ = c.output();
        }
    }
    pids
}

// ---------- 起動 / 停止 ----------

// 対象 port が応答していれば Already。占有していた別プロセスは解放してから
// command を detached spawn（出力は logs/devenv-<id>.log へ追記）
pub async fn launch(root: &Path, e: &DevEnv) -> Result<crate::launch::LaunchResult, String> {
    if e.port != 0 {
        if crate::launch::port_alive(e.port).await {
            return Ok(crate::launch::LaunchResult::Already);
        }
        let killed = kill_port(e.port);
        if !killed.is_empty() {
            // ポート解放を待つ（上限10秒）
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !pids_on_port(e.port).is_empty() {
                if std::time::Instant::now() > deadline {
                    return Err(format!(
                        ":{} が解放されません（残 PID: {:?}）",
                        e.port, killed
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
    if !e.cwd.is_dir() {
        return Err(format!("cwd not found: {}", e.cwd.display()));
    }
    let log = log_path(root, &e.id);
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e2| format!("log open failed: {}", e2))?;
    let err = out.try_clone().map_err(|e2| e2.to_string())?;
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", &e.command])
            .creation_flags(crate::util::CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", &e.command]);
        c
    };
    cmd.current_dir(&e.cwd).stdout(out).stderr(err);
    // 親（kasugai-agent）の PORT を継がせない（子が PORT を読むと
    // 0.0.0.0 バインドやポート衝突になる）。個別指定は env で行う
    cmd.env_remove("PORT");
    for (k, v) in &e.env {
        cmd.env(k, v);
    }
    cmd.spawn()
        .map_err(|e2| format!("launch failed: {}", e2))?;
    Ok(crate::launch::LaunchResult::Launched)
}

// port 占有プロセスを停止。殺した PID 一覧を返す
pub fn stop(e: &DevEnv) -> Result<Vec<u32>, String> {
    if e.port == 0 {
        return Err("port 未指定の環境は停止できません".into());
    }
    Ok(kill_port(e.port))
}
