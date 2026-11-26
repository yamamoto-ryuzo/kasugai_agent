// snapshots.rs — chat が変更したファイルの turn 単位変更前スナップショット採取・復元
// （snapshots.mjs 移植）
//
// chat-snapshots/<chatId>/ 配下にスナップショット本体（sNNNN）と
// manifest.json（エントリ一覧）を置く。エントリは
// { path, turn, snap, existed, ts } — path は正規化済み絶対パス、
// snap は dir 内のファイル名（existed=false の新規作成ファイルは null）。
// 「turn N 開始時点へ戻す」= turn N 以降に触った各ファイルを
// その turn 最古のスナップショットへ書き戻す（当時無ければ削除）。
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::util::{dirname, now_iso};

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub turn: u64,
    pub snap: Option<String>,
    pub existed: bool,
    pub ts: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Manifest {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub prompts: HashMap<String, PromptInfo>,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PromptInfo {
    pub text: String,
    pub ts: String,
}

#[derive(Serialize)]
pub struct RestoreResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub turn: u64,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub failed: Vec<String>,
}

pub struct SnapshotStore {
    dir: PathBuf,
    m: Manifest,
}

impl SnapshotStore {
    pub fn new(dir: PathBuf) -> Self {
        let m = Self::load(&dir);
        Self { dir, m }
    }

    fn load(dir: &Path) -> Manifest {
        if let Ok(s) = std::fs::read_to_string(dir.join("manifest.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                if v.get("entries").and_then(|e| e.as_array()).is_some() {
                    if let Ok(m) = serde_json::from_value::<Manifest>(v) {
                        return m;
                    }
                }
            }
        }
        Manifest::default()
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(s) = serde_json::to_string_pretty(&self.m) {
            let _ = std::fs::write(self.dir.join("manifest.json"), s);
        }
    }

    /// turn 開始時に呼ぶ。prompt 表示用に先頭を記録する
    pub fn begin_turn(&mut self, turn: u64, text: &str) {
        let key = turn.to_string();
        if !self.m.prompts.contains_key(&key) {
            self.m.prompts.insert(
                key,
                PromptInfo {
                    text: text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect(),
                    ts: now_iso(),
                },
            );
            self.save();
        }
    }

    /// path（正規化済み）の現行内容を turn のスナップショットとして採取。
    /// 同一 turn 内では最初の1回のみ
    pub fn capture(&mut self, path: &str, turn: u64) {
        if path.is_empty()
            || self.m.entries.iter().any(|e| e.path == path && e.turn == turn)
        {
            return;
        }
        let existed = Path::new(path).exists();
        let mut snap = None;
        if existed {
            self.m.seq += 1;
            let name = format!("s{:04}", self.m.seq);
            let _ = std::fs::create_dir_all(&self.dir);
            if std::fs::copy(path, self.dir.join(&name)).is_err() {
                self.m.seq -= 1;
                return;
            }
            snap = Some(name);
        }
        self.m.entries.push(Entry {
            path: path.to_string(),
            turn,
            snap,
            existed,
            ts: now_iso(),
        });
        self.save();
    }

    /// 復元ポイント一覧: [{turn, prompt, files, ts}]（turn 昇順）
    pub fn points(&self) -> Vec<serde_json::Value> {
        let mut order: Vec<u64> = Vec::new();
        let mut files: HashMap<u64, Vec<String>> = HashMap::new();
        let mut ts: HashMap<u64, String> = HashMap::new();
        for e in &self.m.entries {
            if !files.contains_key(&e.turn) {
                order.push(e.turn);
                ts.insert(e.turn, e.ts.clone());
            }
            let f = files.entry(e.turn).or_default();
            if !f.contains(&e.path) {
                f.push(e.path.clone());
            }
        }
        order.sort_unstable();
        order
            .into_iter()
            .map(|t| {
                json!({
                    "turn": t,
                    "prompt": self.m.prompts.get(&t.to_string()).map(|p| p.text.clone()).unwrap_or_default(),
                    "files": files.get(&t).cloned().unwrap_or_default(),
                    "ts": ts.get(&t).cloned().unwrap_or_default(),
                })
            })
            .collect()
    }

    pub fn last_turn(&self) -> u64 {
        let e = self.m.entries.iter().map(|e| e.turn).max().unwrap_or(0);
        let p = self
            .m
            .prompts
            .keys()
            .filter_map(|k| k.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        e.max(p)
    }

    /// turn N 以降の変更を元に戻す。None 時は最新ポイントのみ。
    /// 各ファイルは turn>=N の最古エントリの内容へ戻し、当時無ければ削除する。
    /// 戻したエントリ（及び孤立したスナップショット実体）は消費して破棄する
    pub fn restore(&mut self, turn: Option<u64>) -> RestoreResult {
        let pts = self.points();
        let fail = |error: String| RestoreResult {
            ok: false,
            error: Some(error),
            turn: 0,
            restored: vec![],
            deleted: vec![],
            failed: vec![],
        };
        if pts.is_empty() {
            return fail("復元ポイントがありません".into());
        }
        let t = turn.unwrap_or_else(|| {
            pts.last().and_then(|p| p["turn"].as_u64()).unwrap_or(0)
        });
        if !pts.iter().any(|p| p["turn"].as_u64() == Some(t)) {
            return fail(format!("turn {} の復元ポイントがありません", t));
        }
        let mut res = RestoreResult {
            ok: true,
            error: None,
            turn: t,
            restored: vec![],
            deleted: vec![],
            failed: vec![],
        };
        // path -> turn>=t の最古エントリ
        let mut by_path: HashMap<&str, &Entry> = HashMap::new();
        for e in &self.m.entries {
            if e.turn >= t && !by_path.contains_key(e.path.as_str()) {
                by_path.insert(e.path.as_str(), e);
            }
        }
        for e in by_path.values() {
            if !e.existed {
                match std::fs::remove_file(&e.path) {
                    Ok(_) => res.deleted.push(e.path.clone()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        res.deleted.push(e.path.clone())
                    }
                    Err(err) => res.failed.push(format!("{}: {}", e.path, err)),
                }
            } else {
                let _ = std::fs::create_dir_all(dirname(&e.path));
                match std::fs::copy(self.dir.join(e.snap.as_deref().unwrap_or("")), &e.path) {
                    Ok(_) => res.restored.push(e.path.clone()),
                    Err(err) => res.failed.push(format!("{}: {}", e.path, err)),
                }
            }
        }
        let kept: Vec<Entry> = self
            .m
            .entries
            .iter()
            .filter(|e| e.turn < t)
            .cloned()
            .collect();
        let keep_snaps: std::collections::HashSet<String> = kept
            .iter()
            .filter_map(|e| e.snap.clone())
            .collect();
        self.m.entries = kept;
        let drop_keys: Vec<String> = self
            .m
            .prompts
            .keys()
            .filter(|k| k.parse::<u64>().map(|v| v >= t).unwrap_or(false))
            .cloned()
            .collect();
        for k in drop_keys {
            self.m.prompts.remove(&k);
        }
        let snap_re = regex::Regex::new(r"^s\d+$").unwrap();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if snap_re.is_match(&name) && !keep_snaps.contains(&name) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.save();
        res
    }
}
