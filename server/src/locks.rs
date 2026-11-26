// locks.rs — 実機リソースの排他ロック（locks/ ディレクトリのロックファイル）
// verify L2+ が使う QGIS harness・固定ポート等を課題 run 間で排他する
// （README §5.5、AGENTS.md「実機リソースは排他利用」）。
// ロックは locks/<name>.lock（内容 {name, pid, at}）。取得は create_new の
// アトミック作成で行い、保持者 pid の生存確認で stale ロックを回収する。
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::load_j;
use crate::util::now_iso;

// pid 不明・形式不正な残骸を回収するまでの猶予（生成途中クラッシュ対策）
const STALE_UNKNOWN_MS: u128 = 60_000;

pub fn dir(root: &Path) -> PathBuf {
    root.join("locks")
}

fn lock_path(root: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    dir(root).join(format!("{}.lock", safe))
}

// pid が生存しているか（Windows: tasklist / その他: /proc）
fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("tasklist");
        c.args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"]);
        crate::util::hide_console(&mut c);
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

enum State {
    Free,
    Locked,
    Stale,
}

fn state(root: &Path, name: &str) -> State {
    let p = lock_path(root, name);
    if !p.exists() {
        return State::Free;
    }
    let v = load_j(&p, Value::Null);
    if let Some(pid) = v.get("pid").and_then(|x| x.as_u64()) {
        return if pid_alive(pid as u32) {
            State::Locked
        } else {
            State::Stale
        };
    }
    // 形式不明（生成途中クラッシュ等）: 新しければ Locked、古い残骸は Stale
    match p.metadata().and_then(|m| m.modified()) {
        Ok(t) if t.elapsed().unwrap_or_default().as_millis() < STALE_UNKNOWN_MS => State::Locked,
        _ => State::Stale,
    }
}

/// ロック中か（保持者プロセスが生存している場合のみ true）
pub fn is_locked(root: &Path, name: &str) -> bool {
    matches!(state(root, name), State::Locked)
}

/// name のロックを取得。取得成功 true / 他者が保持中なら false。
/// stale（保持者死亡・形式不正の古い残骸）は回収して再試行する。
pub fn acquire(root: &Path, name: &str) -> bool {
    let _ = std::fs::create_dir_all(dir(root));
    let p = lock_path(root, name);
    match state(root, name) {
        State::Locked => return false,
        State::Stale => {
            let _ = std::fs::remove_file(&p);
        }
        State::Free => {}
    }
    let body = json!({"name": name, "pid": std::process::id(), "at": now_iso()});
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
    {
        Ok(mut f) => {
            let _ = f.write_all(body.to_string().as_bytes());
            true
        }
        Err(_) => false, // 競合で他者が先に作成
    }
}

/// name のロックを解放（無くてもよい）
pub fn release(root: &Path, name: &str) {
    let _ = std::fs::remove_file(lock_path(root, name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-locks-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn acquire_release_cycle() {
        let root = temp_root("cycle");
        assert!(acquire(&root, "machine"));
        assert!(is_locked(&root, "machine"));
        // 二重取得は失敗
        assert!(!acquire(&root, "machine"));
        release(&root, "machine");
        assert!(!is_locked(&root, "machine"));
        assert!(acquire(&root, "machine"));
        release(&root, "machine");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let root = temp_root("stale");
        let p = lock_path(&root, "machine");
        std::fs::create_dir_all(dir(&root)).unwrap();
        // 存在しない pid の保持者（999999 はほぼ確実に非生存）
        std::fs::write(&p, json!({"pid": 999999, "at": now_iso()}).to_string()).unwrap();
        assert!(!is_locked(&root, "machine"));
        assert!(acquire(&root, "machine"));
        assert!(is_locked(&root, "machine"));
        release(&root, "machine");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_sanitized() {
        let root = temp_root("name");
        assert!(acquire(&root, "exe:C:/x y/qgis.exe"));
        assert!(is_locked(&root, "exe:C:/x y/qgis.exe"));
        assert!(!is_locked(&root, "other"));
        release(&root, "exe:C:/x y/qgis.exe");
        let _ = std::fs::remove_dir_all(&root);
    }
}
