// main.rs — kasugai_agent control サーバ（control.mjs の起動部に相当）
//
// 使い方:
//   kasugai-agent                        control を 127.0.0.1:8586 で起動
//   kasugai-agent --open-browser         起動後にパネルを既定ブラウザで開く
//   kasugai-agent --respawn <pid> <exe>  （再起動用 detached 子: 旧プロセスの
//                                        終了を待って exe を新規起動する）
//   kasugai-agent --install-task         タスクスケジューラ KasugaiAgentDispatcher
//                                        （ONLOGON 自動起動）を登録
//   kasugai-agent --uninstall-task       上記タスクを削除
//   kasugai-agent --register-protocol    kasugai-agent:// プロトコルを HKCU 登録
//   kasugai-agent --uri "<url>"          プロトコルハンドラ入口
//                                        （kasugai-agent:launch?target=..&port=..）
//   kasugai-agent --import-github        config.repos の GitHub Issue を
//                                        一括取込（open→queued/gated・
//                                        closed→done/rejected の終端起票）
// release ビルドはコンソール窓を出さない（タスクスケジューラ常駐用）。
// CLI サブコマンドは attach_console() で親コンソールへ出力する
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod cases;
mod chat;
mod claims;
mod conlog;
mod config;
mod consolidate;
mod control;
mod detector_health;
mod detectors;
mod github;
mod heartbeat;
mod housekeep;
mod knowledge;
mod launch;
mod lessons;
mod locks;
mod pipeline;
mod proposals;
mod registry;
mod repomatch;
mod runner;
mod snapshots;
mod split;
mod store;
mod util;
mod verify;

use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::Instant;

use chat::CtrlEvt;
use config::*;
use conlog::conlog;
use control::{App, Primed};

// pid が生存しているか（Windows: tasklist で判定）
fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("tasklist");
        c.args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"]);
        util::hide_console(&mut c);
        if let Ok(out) = c.output() {
            let s = String::from_utf8_lossy(&out.stdout);
            return s.contains(&format!(",\"{}\",", pid))
                || s.contains(&format!("\"{}\",", pid));
        }
        false
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{}", pid)).exists()
    }
}

// --respawn: 旧 PID の終了をポーリング（300ms・上限120s）してから exe を起動
fn respawn(old_pid: u32, exe: &Path) -> i32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while pid_alive(old_pid) {
        if std::time::Instant::now() > deadline {
            return 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        // 隠し console を割り当てる（DETACHED_PROCESS だと孫プロセスに
        // 見える窓が割り当てられる）。子はこれを継承するので窓が出ない
        if std::process::Command::new(exe)
            .creation_flags(util::CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .is_ok()
        {
            return 0;
        }
    }
    #[cfg(not(windows))]
    {
        if std::process::Command::new(exe).spawn().is_ok() {
            return 0;
        }
    }
    1
}

// windows subsystem（release）では標準出力が無いため、CLI サブコマンド時は
// 親コンソールへ attach して println!/eprintln! を届ける
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
#[cfg(not(windows))]
fn attach_console() {}

// ROOT: exe の2階層上（server/target/<profile>/kasugai-agent.exe → リポジトリ根）
// 開発時は CARGO_MANIFEST_DIR（server/）の親。exe が server/bin 等へ配布される
// 構成では exe 近接の config.json を優先する
fn find_root() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // exe 隣接に config.json がある構成（配布版）を優先
    let adj = exe_dir.join("config.json");
    if adj.is_file() {
        exe_dir
    } else {
        manifest_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--respawn") {
        let pid: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let exe = PathBuf::from(args.get(3).cloned().unwrap_or_default());
        std::process::exit(respawn(pid, &exe));
    }

    // CLI サブコマンド（即終了系）。プロトコル呼び出しは URL が第1引数に来る
    let arg1 = args.get(1).map(|s| s.as_str());
    let uri_arg = match arg1 {
        Some("--uri") => args.get(2).map(|s| s.as_str()),
        Some(a) if a.starts_with(&format!("{}:", launch::PROTOCOL)) => Some(a),
        _ => None,
    };
    if let Some(uri) = uri_arg {
        attach_console();
        let code = launch::handle_uri(uri, &find_root()).await;
        std::process::exit(code);
    }
    match arg1 {
        Some("--install-task") => {
            attach_console();
            let exe = std::env::current_exe().unwrap_or_default();
            match launch::install_task(&exe) {
                Ok(m) => println!("{}", m),
                Err(e) => {
                    eprintln!("install-task failed: {}", e);
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        Some("--uninstall-task") => {
            attach_console();
            match launch::uninstall_task() {
                Ok(m) => println!("{}", m),
                Err(e) => {
                    eprintln!("uninstall-task failed: {}", e);
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        Some("--register-protocol") => {
            attach_console();
            match std::env::current_exe() {
                Ok(exe) => {
                    launch::register_protocol(&exe);
                    println!("{}:// を登録しました", launch::PROTOCOL);
                }
                Err(e) => {
                    eprintln!("current_exe failed: {}", e);
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        Some("--import-github") => {
            attach_console();
            let root = find_root();
            let cfg = load_config(&root);
            let (created, total) =
                detectors::github_issues::import_all(&root, &cfg).await;
            println!(
                "github issue 取込: {} 件中 {} 件を新規起票（既出は lastSeen 更新のみ）",
                total, created
            );
            std::process::exit(0);
        }
        _ => {}
    }

    let root = find_root();
    std::env::set_current_dir(&root).ok();

    // kasugai-agent:// を OS へ自己登録（exe パス変更多登録の追従も兼ねて毎回）
    if let Ok(exe) = std::env::current_exe() {
        launch::register_protocol(&exe);
    }

    let cfg = load_config(&root);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| cfg.get("controlPort").and_then(|v| v.as_u64()).map(|v| v as u16))
        .unwrap_or(8586);

    let ws_root = cfg
        .get("workspaceRoot")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            root.parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| ".".to_string())
        });
    let ws_root = util::norm_slashes(&ws_root);

    let (sse_tx, _) = tokio::sync::broadcast::channel(512);
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<CtrlEvt>();

    let app = Arc::new(App {
        root: root.clone(),
        panel: root.join("panel"),
        uploads: root.join("chat-uploads"),
        snaps: root.join("chat-snapshots"),
        ws_root,
        cfg: tokio::sync::RwLock::new(cfg.clone()),
        port,
        chats: tokio::sync::Mutex::new(BTreeMap::new()),
        chat_seq: AtomicI64::new(0),
        sse: sse_tx,
        evt_tx,
        started: Instant::now(),
        restart_pending: std::sync::atomic::AtomicBool::new(false),
        restarting: std::sync::atomic::AtomicBool::new(false),
        busy_prev: tokio::sync::Mutex::new(HashMap::new()),
        assets: tokio::sync::Mutex::new(assets::Mgr::new()),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3600))
            .build()
            .unwrap_or_default(),
    });

    // chats.json から復元
    {
        let saved = load_j(&root.join("chats.json"), json!({}));
        if let Some(obj) = saved.as_object() {
            for (id, s) in obj {
                let Ok(i64id) = id.parse::<i64>() else { continue };
                let mut seen_splits = HashSet::new();
                if let Some(arr) = s.get("seenSplits").and_then(|v| v.as_array()) {
                    for v in arr {
                        if let Some(x) = v.as_str() {
                            seen_splits.insert(x.to_string());
                        }
                    }
                }
                let mut repo_declined = HashSet::new();
                if let Some(arr) = s.get("repoDeclined").and_then(|v| v.as_array()) {
                    for v in arr {
                        if let Some(x) = v.as_str() {
                            repo_declined.insert(x.to_string());
                        }
                    }
                }
                let mut pending_splits = Vec::new();
                if let Some(arr) = s.get("pendingSplits").and_then(|v| v.as_array()) {
                    for v in arr {
                        let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
                        let prompt = v.get("prompt").and_then(|x| x.as_str()).unwrap_or("");
                        if !title.is_empty() && !prompt.is_empty() {
                            pending_splits.push(split::Split {
                                title: title.to_string(),
                                prompt: prompt.to_string(),
                            });
                        }
                    }
                }
                app.new_chat(control::NewChatOpts {
                    label: s.get("label").and_then(|v| v.as_str()).map(String::from),
                    repo: s.get("repo").cloned().filter(|v| !v.is_null()),
                    agent: s.get("agent").and_then(|v| v.as_str()).map(String::from),
                    id: Some(i64id),
                    session_id: s
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    updated_at: s
                        .get("updatedAt")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    mode: s.get("mode").and_then(|v| v.as_str()).map(String::from),
                    parent: s.get("parent").and_then(|v| v.as_i64()),
                    depth: s.get("depth").and_then(|v| v.as_u64()).unwrap_or(0),
                    primed_for: Some(Primed::from_persisted(s.get("primedFor"))),
                    seen_splits,
                    pending_splits,
                    repo_declined,
                    verify: s.get("verify").and_then(|v| v.as_str()).map(String::from),
                    verify_count: s.get("verifyCount").and_then(|v| v.as_u64()).unwrap_or(0),
                    verify_back: s.get("verifyBack").and_then(|v| v.as_u64()).unwrap_or(0),
                    done_kind: s
                        .get("doneKind")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    back_prompt: s
                        .get("backPrompt")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    needs_human: s
                        .get("needsHuman")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    usage: s
                        .get("usage")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok()),
                    // 再起動前の待機状態（kind=repo の確認待ち・保留 prompt を含む）
                    waiting: s.get("waiting").and_then(|v| {
                        serde_json::from_value::<Option<chat::WaitState>>(v.clone())
                            .ok()
                            .flatten()
                    }),
                    held_prompts: s
                        .get("heldPrompts")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    resume_on_release: s
                        .get("resumeOnRelease")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                })
                .await;
            }
        }
    }

    // イベントループ + 監視タスク
    {
        let app2 = app.clone();
        tokio::spawn(async move { control::event_loop(app2, evt_rx).await });
    }
    control::spawn_watchers(app.clone());

    // pendingSplits が残る親 chat は起動時に drain を試行する。
    // 再起動で子の busy エッジ（完了検知）が消えるため、これがないと
    // 待機キューが誰にも起動されず滞留する
    {
        let app2 = app.clone();
        tokio::spawn(async move {
            let ids: Vec<i64> = {
                let chats = app2.chats.lock().await;
                chats
                    .iter()
                    .filter(|(_, s)| !s.pending_splits.is_empty())
                    .map(|(id, _)| *id)
                    .collect()
            };
            for id in ids {
                app2.spawn_splits(id).await;
            }
        });
    }

    // 起動時の自動再開: resumable な chat を全て ensureSession で再接続
    if cfg_bool_default_true(&cfg, "autoResumeOnStart") {
        let app2 = app.clone();
        tokio::spawn(async move {
            let mut todo: Vec<chat::Chat> = Vec::new();
            {
                let chats = app2.chats.lock().await;
                for s in chats.values() {
                    if s.chat.core().lock().await.saved_session_id.is_some() {
                        todo.push(s.chat.clone());
                    }
                }
            }
            for chat in todo {
                if chat.has_proc().await {
                    continue;
                }
                if let Err(e) = chat.ensure_session().await {
                    chat.push_sys(&format!("[自動再開失敗] {}", e)).await;
                }
            }
            // 起動時の自動再処理: 差し戻し待ち・検証待ち・宙ぶら待機・
            // 起動失敗の保留 prompt を再送（再開可への続行 prompt・
            // 要人対応・repo 確認待ちは対象外）
            app2.resume_all(None, false).await;
        });
    }

    // 起動
    let router = control::router(app.clone());
    let addr = format!("127.0.0.1:{}", port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            // 多重起動防止: 既存インスタンスが応答するならパネルを開いて終了
            // （実行中エージェントを kill しない。exe 更新の再起動は
            //   autoRestartOnDrift / /api/control/restart の既存経路が担う）
            let existing = reqwest::get(&format!("http://{}/api/state", addr))
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if existing {
                println!(
                    "[kasugai_agent] 既に :{} で稼働中。パネルを開きます",
                    port
                );
                util::open_browser(&format!("http://{}/", addr));
                std::process::exit(0);
            }
            conlog("control", "err", &format!("bind {} 失敗: {}", addr, e));
            eprintln!("[kasugai_agent] bind {} failed: {}", addr, e);
            std::process::exit(1);
        }
    };
    conlog(
        "control",
        "sys",
        &format!("[kasugai_agent] control :{}  http://{}/", port, addr),
    );
    println!("[kasugai_agent] control :{}  http://{}/", port, addr);
    if args.iter().any(|a| a == "--open-browser") {
        let url = format!("http://{}/", addr);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            util::open_browser(&url);
        });
    }
    axum::serve(listener, router).await.ok();
}
