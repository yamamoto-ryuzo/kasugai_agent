// util.rs — パス正規化・時刻・乱数などの横断ユーティリティ
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// JS Date#toISOString 相当（UTC・ミリ秒・Z サフィックス）
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// パス区切りを `/` に揃え末尾の `/` を除去（JS normP 相当）
pub fn norm_slashes(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_string()
}

/// lexically なパス正規化（. / .. を文字列上で解決）
pub fn normalize_lexical(path: &str) -> String {
    let is_rooted = path.starts_with('/') || path.get(1..2) == Some(":");
    let mut out: Vec<String> = Vec::new();
    for seg in path.replace('\\', "/").split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                out.pop();
            }
            s => out.push(s.to_string()),
        }
    }
    let joined = out.join("/");
    if is_rooted {
        // Windows: "C:" セグメントは保持されるので先頭に / は要らない
        if path.starts_with('/') {
            format!("/{}", joined)
        } else {
            joined
        }
    } else {
        joined
    }
}

/// claims.mjs の normPath: cwd 基準で絶対化し小文字化
pub fn norm_path(p: &str, cwd: &str) -> String {
    let s = norm_slashes(p.trim());
    if s.is_empty() {
        return String::new();
    }
    let abs = if s.get(1..2) == Some(":") || s.starts_with('/') {
        s
    } else {
        format!("{}/{}", norm_slashes(cwd), s)
    };
    normalize_lexical(&abs).to_lowercase()
}

/// std::fs::canonicalize 相当だが存在しないパスでも文字列解決する
pub fn resolve(base: &str, rel: &str) -> String {
    let rel_n = rel.replace('\\', "/");
    if rel_n.get(1..2) == Some(":") || rel_n.starts_with('/') {
        return normalize_lexical(&rel_n);
    }
    normalize_lexical(&format!("{}/{}", base.trim_end_matches('/'), rel_n))
}

pub fn basename(p: &str) -> String {
    let n = norm_slashes(p);
    n.rsplit('/').next().unwrap_or("").to_string()
}

pub fn dirname(p: &str) -> String {
    let n = norm_slashes(p);
    match n.rfind('/') {
        Some(0) | None => n,
        Some(i) => n[..i].to_string(),
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn appdata() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
}

// ---- 乱数（暗号強度不要：upload名・symptomHash 用） ----
static RNG: AtomicU64 = AtomicU64::new(0);

pub fn rand_hex(bytes: usize) -> String {
    let seed = RNG.load(Ordering::Relaxed);
    let s = if seed == 0 {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e3779b97f4a7c15);
        let v = t ^ 0x9e3779b97f4a7c15;
        RNG.compare_exchange(0, v, Ordering::Relaxed, Ordering::Relaxed)
            .ok();
        v
    } else {
        seed
    };
    let mut x = RNG
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
        })
        .unwrap_or(s)
        .wrapping_add(1);
    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(&format!("{:016x}", x));
    }
    out.truncate(bytes * 2);
    out
}

/// control.mjs normP: 小文字化 + 末尾スラッシュ除去（比較用キー）
pub fn norm_p(p: &str) -> String {
    norm_slashes(p).to_lowercase()
}

/// Node path.normalize 相当（lexical 解決のみ・絶対化しない）
pub fn normalize(p: &str) -> String {
    normalize_lexical(p)
}

pub fn now_ms() -> i64 {
    now_millis()
}

pub fn is_dir(p: &str) -> bool {
    Path::new(p).is_dir()
}
pub fn is_file(p: &str) -> bool {
    Path::new(p).is_file()
}
pub fn exists(p: &str) -> bool {
    Path::new(p).exists()
}

// ---- Windows: 子プロセスにコンソール窓を作らせない（Node windowsHide:true 相当） ----
// detached / windowsHide で起動された control はコンソールを持たないため、
// フラグ無しで spawn した console アプリには見える窓が割り当てられてしまう
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
pub fn hide_console(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub fn hide_console(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

#[cfg(windows)]
pub fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd
}

// 既定ブラウザで URL を開く（多重起動時のパネル表示・--open-browser 用）
#[cfg(windows)]
pub fn open_browser(url: &str) {
    let mut c = std::process::Command::new("cmd");
    c.args(["/c", "start", "", url]);
    hide_console(&mut c);
    let _ = c.spawn();
}
#[cfg(not(windows))]
pub fn open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// コミットメッセージの `Issue: <key>` トレーラを抽出（runner の帰属整理と
/// ci-failure detector の親課題リンクで共有。複数あれば最後の1件。
/// `issue:` 小文字始まりも許容）
pub fn issue_trailer(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("Issue:")
                .or_else(|| l.strip_prefix("issue:"))
        })
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
        .last()
}

/// Component ベースの相対化（registry/refs 用）
pub fn relative_path(from: &str, to: &str) -> Option<String> {
    let a: Vec<Component> = Path::new(from).components().collect();
    let b: Vec<Component> = Path::new(to).components().collect();
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); a.len() - i];
    parts.extend(b[i..].iter().filter_map(|c| match c {
        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    }));
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// git 実行（60s timeout。control の git ナビ・detectors の双方で共有。
/// 戻り値は (終了コード, stdout+stderr の結合)。起動失敗は -1）
pub async fn run_git(cwd: &str, args: &[&str]) -> (i32, String) {
    use tokio::io::AsyncReadExt;
    let mut gcmd = tokio::process::Command::new("git");
    gcmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_tokio(&mut gcmd);
    let child = gcmd.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (-1, e.to_string()),
    };
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let ho = tokio::spawn(async move {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v).await;
        v
    });
    let he = tokio::spawn(async move {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v).await;
        v
    });
    let res = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait()).await;
    match res {
        Ok(Ok(st)) => {
            let o = ho.await.unwrap_or_default();
            let e = he.await.unwrap_or_default();
            (
                st.code().unwrap_or(-1),
                format!("{}{}", String::from_utf8_lossy(&o), String::from_utf8_lossy(&e))
                    .trim()
                    .to_string(),
            )
        }
        Ok(Err(e)) => (-1, e.to_string()),
        Err(_) => {
            let _ = child.kill().await;
            (-1, "timeout".to_string())
        }
    }
}
