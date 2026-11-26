// launch.rs — 環境起動ロジックの共有層
// 正本: diagrams/env-launch.mmd → README §8「環境起動」
//   - docs ナビの起動ボタン: POST /api/control/launch（control.rs::api_launch）
//   - kasugai-agent: プロトコル: kasugai-agent.exe --uri "<url>"（main.rs）
//   - ログオン自動起動: HKCU Run キー KasugaiAgentDispatcher（prod 常駐）

use std::path::Path;

pub const TASK_NAME: &str = "KasugaiAgentDispatcher";
pub const PROTOCOL: &str = "kasugai-agent";

pub enum LaunchResult {
    Already,
    Launched,
}

// ---------- 環境起動（dev / prod） ----------

pub async fn port_alive(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

// detached spawn + env（環境起動用）
pub fn spawn_detached_env(
    exe: &Path,
    args: &[String],
    dir: &Path,
    port: u16,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        std::process::Command::new(exe)
            .args(args)
            .current_dir(dir)
            .env("PORT", port.to_string())
            .creation_flags(crate::util::CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(exe)
            .args(args)
            .current_dir(dir)
            .env("PORT", port.to_string())
            .spawn()?;
    }
    Ok(())
}

// 対象ポートが既に応答していれば起動せず Already を返す。
// 起動するのは常に prod exe（release ビルド。無ければ現行 exe）。
// 開発実行は cargo run（フォアグラウンド）が入口で、detached 起動の対象外
pub async fn launch_env(root: &Path, target: &str, port: u16) -> Result<LaunchResult, String> {
    if target != "prod" {
        return Err("unknown target".into());
    }
    if port_alive(port).await {
        return Ok(LaunchResult::Already);
    }
    let rel = root
        .join("server")
        .join("target")
        .join("release")
        .join("kasugai-agent.exe");
    let exe = match rel.is_file() {
        true => rel,
        false => std::env::current_exe().map_err(|e| format!("current_exe failed: {}", e))?,
    };
    spawn_detached_env(&exe, &[], root, port)
        .map(|_| LaunchResult::Launched)
        .map_err(|e| format!("launch failed: {}", e))
}

// ---------- kasugai-agent:// プロトコル ----------

// exe 起動時に HKCU へ自己登録（管理者権限不要）。失敗は起動を妨げないため無視
pub fn register_protocol(exe: &Path) {
    #[cfg(windows)]
    {
        let base = format!("HKCU\\Software\\Classes\\{}", PROTOCOL);
        let open = format!("{}\\shell\\open\\command", base);
        let cmd_line = format!("\"{}\" --uri \"%1\"", exe.to_string_lossy());
        let ops: Vec<Vec<String>> = vec![
            vec![
                "add".into(),
                base.clone(),
                "/ve".into(),
                "/d".into(),
                "URL:kasugai-agent Protocol".into(),
                "/f".into(),
            ],
            vec![
                "add".into(),
                base,
                "/v".into(),
                "URL Protocol".into(),
                "/d".into(),
                "".into(),
                "/f".into(),
            ],
            vec![
                "add".into(),
                open,
                "/ve".into(),
                "/d".into(),
                cmd_line,
                "/f".into(),
            ],
        ];
        for args in ops {
            let mut c = std::process::Command::new("reg");
            c.args(&args);
            crate::util::hide_console(&mut c);
            let _ = c.output();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
    }
}

// 簡易 %XX デコード（query 値用）
fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// "kasugai-agent:launch?port=8586"（// 形式も許容）を解釈して起動。
// プロトコル起動時はブラウザがハンドラの終了を待つので spawn 後は即終了する
pub async fn handle_uri(uri: &str, root: &Path) -> i32 {
    let body = uri
        .strip_prefix(&format!("{}:", PROTOCOL))
        .unwrap_or(uri)
        .trim_start_matches('/');
    let (action, query) = body.split_once('?').unwrap_or((body, ""));
    let get = |k: &str| -> Option<String> {
        query.split('&').find_map(|p| {
            let (a, b) = p.split_once('=')?;
            (a == k).then(|| url_decode(b))
        })
    };
    match action {
        "launch" => {
            let target = get("target").unwrap_or_else(|| "prod".into());
            let port = get("port")
                .and_then(|s| s.parse().ok())
                .unwrap_or(8586);
            match launch_env(root, &target, port).await {
                Ok(_) => 0,
                Err(e) => {
                    eprintln!("[kasugai_agent] launch failed: {}", e);
                    1
                }
            }
        }
        _ => {
            eprintln!("[kasugai_agent] unknown uri: {}", uri);
            2
        }
    }
}

// ---------- ログオン自動起動（HKCU Run キー） ----------
// 非管理者ではタスクスケジューラへ登録できないため、同一目的を
// HKCU\...\Run\KasugaiAgentDispatcher で実現する（reg add、権限不要）

#[cfg(windows)]
const RUN_KEY: &str = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn reg(args: &[String]) -> std::io::Result<std::process::Output> {
    let mut c = std::process::Command::new("reg");
    c.args(args);
    crate::util::hide_console(&mut c);
    c.output()
}

// Run キーに登録されているコマンド文字列（未登録は None）
#[cfg(windows)]
fn task_registered_cmd() -> Option<String> {
    let out = reg(&[
        "query".into(),
        RUN_KEY.into(),
        "/v".into(),
        TASK_NAME.into(),
    ])
    .ok()?;
    if !out.status.success() {
        return None;
    }
    // 出力例: "    KasugaiAgentDispatcher    REG_SZ    \"C:\\...\\kasugai-agent.exe\""
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .find_map(|l| l.split("REG_SZ").nth(1).map(|v| v.trim().to_string()))
}

pub fn task_installed() -> bool {
    #[cfg(windows)]
    {
        task_registered_cmd().is_some()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn install_task(exe: &Path) -> Result<String, String> {
    #[cfg(windows)]
    {
        let cmd = format!("\"{}\"", exe.to_string_lossy());
        let out = reg(&[
            "add".into(),
            RUN_KEY.into(),
            "/v".into(),
            TASK_NAME.into(),
            "/t".into(),
            "REG_SZ".into(),
            "/d".into(),
            cmd,
            "/f".into(),
        ])
        .map_err(|e| e.to_string())?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if out.status.success() {
            Ok(format!("registered: {}", TASK_NAME))
        } else {
            Err(text.trim().to_string())
        }
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err("unsupported on this OS".into())
    }
}

pub fn uninstall_task() -> Result<String, String> {
    #[cfg(windows)]
    {
        let out = reg(&[
            "delete".into(),
            RUN_KEY.into(),
            "/v".into(),
            TASK_NAME.into(),
            "/f".into(),
        ])
        .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(format!("unregistered: {}", TASK_NAME))
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
            .trim()
            .to_string())
        }
    }
    #[cfg(not(windows))]
    {
        Err("unsupported on this OS".into())
    }
}

// 自動起動の表示用ステータス（api_state.autostart / GET /api/control/autostart）
// cmd は Run キーに登録されている実コマンド（どの exe が自動起動されるか）
pub fn autostart_status() -> serde_json::Value {
    #[cfg(windows)]
    let cmd = task_registered_cmd();
    #[cfg(not(windows))]
    let cmd: Option<String> = None;
    serde_json::json!({
        "task": TASK_NAME,
        "installed": cmd.is_some(),
        "cmd": cmd,
        "exe": std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    })
}
