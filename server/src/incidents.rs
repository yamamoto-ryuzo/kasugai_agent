// incidents.rs — detector 横断の事案（incident）集約レイヤ
// （正本: diagrams/incidents.mmd、README §5.2.1、manual-343b5d37）
//
// dedupe は完全一致の重複排除だが、複数 detector の症状が時間的・因果的に
// 隣接する場合は1事案（incident）として扱う。事案は incidents.json に
// 記録され、課題は incident フィールドで所属先 id を指す。
//
//   incident レコード:
//     {id:"inc-<hex>", title, auto:bool, createdAt, updatedAt,
//      members:[issueKey...]}
//
// - 自動集約: intake 起票後に auto_assign が関連 open 課題を探す。
//   明示リンク（splitFrom）は常に同一事案。類似は同一 repo +
//   incidentWindowMinutes（既定60・0=無効）内 + cases::score>=SIMILAR_MIN
// - 手動 merge/unmerge: 選択課題群を1事案へ結合 / 事案から外す
//   （空の事案は削除）
// - 事案 dispatch: 事案の queued メンバーを一括 dispatch する
//   （dispatchable が対象 key 群を返し、起動自体は既存 dispatch 経路）
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::cases;
use crate::config::{cfg_f64, load_config, load_j, save_j};
use crate::store;
use crate::util::{now_iso, rand_hex};

/// 事案の open 判定（terminal 以外。done/rejected/dropped は終端。
/// failed は人の判断待ちで活動中扱い）
fn is_open(issue: &Value) -> bool {
    !matches!(
        issue.get("status").and_then(|v| v.as_str()),
        Some("done") | Some("rejected") | Some("dropped")
    )
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn ts(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

pub fn incidents_path(root: &Path) -> PathBuf {
    root.join("incidents.json")
}

pub fn load_incidents(root: &Path) -> Vec<Value> {
    load_j(&incidents_path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub fn save_incidents(root: &Path, incs: &[Value]) {
    save_j(&incidents_path(root), &json!(incs));
}

/// 事案 id での検索（index 返し）
fn find_inc(incs: &[Value], id: &str) -> Option<usize> {
    incs.iter()
        .position(|i| i.get("id").and_then(|v| v.as_str()) == Some(id))
}

/// 課題 key が所属する事案の index
fn inc_of(incs: &[Value], issue_key: &str) -> Option<usize> {
    incs.iter().position(|i| {
        i.get("members")
            .and_then(|m| m.as_array())
            .is_some_and(|ms| ms.iter().any(|k| k.as_str() == Some(issue_key)))
    })
}

fn new_incident(title: &str, auto: bool) -> Value {
    let now = now_iso();
    json!({
        "id": format!("inc-{}", rand_hex(4)),
        "title": title,
        "auto": auto,
        "createdAt": now,
        "updatedAt": now,
        "members": [],
    })
}

fn touch(inc: &mut Value) {
    inc.as_object_mut()
        .unwrap()
        .insert("updatedAt".into(), json!(now_iso()));
}

fn member_add(inc: &mut Value, key: &str) {
    let o = inc.as_object_mut().unwrap();
    let mut ms: Vec<Value> = o
        .get("members")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    if !ms.iter().any(|k| k.as_str() == Some(key)) {
        ms.push(json!(key));
        o.insert("members".into(), json!(ms));
    }
    touch(inc);
}

fn member_remove(inc: &mut Value, key: &str) {
    let o = inc.as_object_mut().unwrap();
    let ms: Vec<Value> = o
        .get("members")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|k| k.as_str() != Some(key))
        .collect();
    o.insert("members".into(), json!(ms));
    touch(inc);
}

/// 課題の incident フィールドを設定/解除して保存する
fn set_issue_incident(issues: &mut [Value], key: &str, id: Option<&str>) {
    if let Some(i) = issues
        .iter_mut()
        .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(key))
    {
        let o = i.as_object_mut().unwrap();
        match id {
            Some(v) => o.insert("incident".into(), json!(v)),
            None => o.remove("incident"),
        };
        o.insert("updatedAt".into(), json!(now_iso()));
    }
}

/// 起票直後の自動集約（intake_inner から呼ぶ。incidents.mmd REL）:
/// 明示リンク（splitFrom）は常に相手の事案へ。類似は同一 repo +
/// incidentWindowMinutes 内に検出された open 課題で score>=SIMILAR_MIN
/// の最高得点者。相手が事案所属ならそれへ追加、未所属なら新規事案を作り
/// 両課題を所属化する。該当なしなら何もしない（単独課題）。
/// 戻り値は所属した事案 id（未所属は None）
pub fn auto_assign(root: &Path, issue: &Value) -> Option<String> {
    let cfg = load_config(root);
    let win_min = cfg_f64(&cfg, "incidentWindowMinutes", 60.0);
    if win_min <= 0.0 {
        return None;
    }
    let key = str_of(issue, "key").to_string();
    if key.is_empty() {
        return None;
    }
    let mut issues = store::load_issues(root);
    // 既に事案所属なら触らない（merge 済みの手動所属を上書きしない）
    if issues
        .iter()
        .find(|i| str_of(i, "key") == key)
        .and_then(|i| i.get("incident"))
        .and_then(|v| v.as_str())
        .is_some()
    {
        return None;
    }
    // 関連 open 課題: 明示リンク → 類似（時間窓 + score）の順で1件選ぶ
    let split_from = str_of(issue, "splitFrom").to_string();
    let detected = ts(str_of(issue, "detectedAt")).unwrap_or_else(chrono::Utc::now);
    let win = chrono::Duration::minutes(win_min as i64);
    let related_key = if !split_from.is_empty()
        && issues
            .iter()
            .any(|i| str_of(i, "key") == split_from && is_open(i))
    {
        Some(split_from.clone())
    } else {
        issues
            .iter()
            .filter(|i| {
                str_of(i, "key") != key
                    && is_open(i)
                    && str_of(i, "repo") == str_of(issue, "repo")
                    && ts(str_of(i, "detectedAt"))
                        .map(|t| (t - detected).num_seconds().abs() <= win.num_seconds())
                        .unwrap_or(false)
                    && cases::score(i, issue) >= cases::SIMILAR_MIN
            })
            .max_by_key(|i| cases::score(i, issue))
            .map(|i| str_of(i, "key").to_string())
    };
    let rk = related_key?;
    let title = issues
        .iter()
        .find(|i| str_of(i, "key") == rk)
        .map(|i| str_of(i, "title").to_string())
        .unwrap_or_default();
    let mut incs = load_incidents(root);
    let id = match inc_of(&incs, &rk) {
        Some(ix) => {
            member_add(&mut incs[ix], &key);
            str_of(&incs[ix], "id").to_string()
        }
        None => {
            let mut inc = new_incident(&title, true);
            member_add(&mut inc, &rk);
            member_add(&mut inc, &key);
            let id = str_of(&inc, "id").to_string();
            incs.push(inc);
            id
        }
    };
    save_incidents(root, &incs);
    set_issue_incident(&mut issues, &rk, Some(&id));
    set_issue_incident(&mut issues, &key, Some(&id));
    store::save_issues(root, &issues);
    Some(id)
}

/// 手動 merge（incidents.mmd）: keys の課題群を1事案へ結合する。
/// into 指定時はその事案へ追加。未指定で keys のどれかが既に事案所属
/// ならその事案へ、全員未所属なら新規事案を作る。別事案の所属課題は
/// 移籍（元事案が空になれば削除）。2件以上必須
pub fn merge(root: &Path, keys: &[String], into: Option<&str>) -> Result<Value, String> {
    let keys: Vec<String> = keys
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    if keys.len() < 2 && into.is_none() {
        return Err("2 課題以上の keys が必要".into());
    }
    let mut issues = store::load_issues(root);
    for k in &keys {
        if !issues.iter().any(|i| str_of(i, "key") == k) {
            return Err(format!("issue not found: {}", k));
        }
    }
    let mut incs = load_incidents(root);
    // 宛先事案: into > keys の既存所属 > 新規
    let tix = match into {
        Some(id) if !id.is_empty() => {
            find_inc(&incs, id).ok_or_else(|| "incident not found".to_string())?
        }
        _ => match keys.iter().find_map(|k| inc_of(&incs, k)) {
            Some(ix) => ix,
            None => {
                let title = issues
                    .iter()
                    .find(|i| str_of(i, "key") == keys[0])
                    .map(|i| str_of(i, "title").to_string())
                    .unwrap_or_default();
                let mut inc = new_incident(&title, false);
                member_add(&mut inc, &keys[0]);
                incs.push(inc);
                incs.len() - 1
            }
        },
    };
    let id = str_of(&incs[tix], "id").to_string();
    for k in &keys {
        // 別事案からの移籍（空の元事案は後で掃除）
        if let Some(src) = inc_of(&incs, k) {
            if src != tix {
                member_remove(&mut incs[src], k);
            }
        }
        member_add(&mut incs[tix], k);
        set_issue_incident(&mut issues, k, Some(&id));
    }
    // 空の事案を削除
    incs.retain(|i| {
        i.get("members")
            .and_then(|m| m.as_array())
            .is_some_and(|ms| !ms.is_empty())
    });
    save_incidents(root, &incs);
    store::save_issues(root, &issues);
    let ix = find_inc(&incs, &id).ok_or_else(|| "incident not found".to_string())?;
    Ok(incs[ix].clone())
}

/// 手動 unmerge（incidents.mmd）: 課題を事案から外す。空の事案は削除
pub fn unmerge(root: &Path, key: &str) -> Result<Value, String> {
    let mut incs = load_incidents(root);
    let Some(ix) = inc_of(&incs, key) else {
        return Err("issue is not in an incident".into());
    };
    member_remove(&mut incs[ix], key);
    incs.retain(|i| {
        i.get("members")
            .and_then(|m| m.as_array())
            .is_some_and(|ms| !ms.is_empty())
    });
    let mut issues = store::load_issues(root);
    set_issue_incident(&mut issues, key, None);
    save_incidents(root, &incs);
    store::save_issues(root, &issues);
    Ok(json!({"ok": true, "key": key}))
}

/// 事案の queued メンバー key 群（事案 dispatch の対象。control が
/// 各 key を既存 dispatch 経路で起動する）
pub fn dispatchable(root: &Path, id: &str) -> Result<Vec<String>, String> {
    let incs = load_incidents(root);
    let Some(ix) = find_inc(&incs, id) else {
        return Err("incident not found".into());
    };
    let issues = store::load_issues(root);
    let keys: Vec<String> = incs[ix]
        .get("members")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|k| k.as_str().map(String::from))
        .filter(|k| {
            issues.iter().any(|i| {
                str_of(i, "key") == k
                    && i.get("status").and_then(|v| v.as_str())
                        == Some(crate::pipeline::ST_QUEUED)
            })
        })
        .collect();
    Ok(keys)
}

/// 一覧表示用: 事案 + メンバー課題の最小情報
pub fn list(root: &Path) -> Vec<Value> {
    let issues = store::load_issues(root);
    load_incidents(root)
        .into_iter()
        .map(|mut inc| {
            let members: Vec<Value> = inc
                .get("members")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|k| {
                    issues
                        .iter()
                        .find(|i| Some(str_of(i, "key")) == k.as_str())
                        .map(|i| {
                            json!({
                                "key": str_of(i, "key"),
                                "title": str_of(i, "title"),
                                "status": str_of(i, "status"),
                            })
                        })
                        .or_else(|| Some(json!({"key": k.as_str().unwrap_or(""), "missing": true})))
                })
                .collect();
            inc.as_object_mut()
                .unwrap()
                .insert("memberIssues".into(), json!(members));
            inc
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-incidents-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn issue(key: &str, repo: &str, title: &str, status: &str) -> Value {
        json!({
            "key": key, "repo": repo, "title": title, "body": "b",
            "status": status, "detectedAt": now_iso(),
        })
    }

    #[test]
    fn merge_groups_and_unmerge_restores() {
        let root = temp_root("merge");
        let issues = vec![
            issue("k1", "me/r", "a", "queued"),
            issue("k2", "me/r", "b", "queued"),
            issue("k3", "me/r", "c", "queued"),
        ];
        store::save_issues(&root, &issues);
        let inc = merge(&root, &["k1".into(), "k2".into()], None).unwrap();
        let id = str_of(&inc, "id").to_string();
        assert_eq!(inc["members"].as_array().unwrap().len(), 2);
        let issues = store::load_issues(&root);
        assert_eq!(str_of(&issues[0], "incident"), id);
        assert_eq!(str_of(&issues[1], "incident"), id);
        assert!(issues[2].get("incident").is_none());
        // 追加入会（into）
        let inc = merge(&root, &["k3".into()], Some(&id)).unwrap();
        assert_eq!(inc["members"].as_array().unwrap().len(), 3);
        // unmerge で外すと空でなければ事案は残る
        unmerge(&root, "k3").unwrap();
        let issues = store::load_issues(&root);
        assert!(issues[2].get("incident").is_none());
        // 全員抜けると事案削除
        unmerge(&root, "k1").unwrap();
        unmerge(&root, "k2").unwrap();
        assert!(load_incidents(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_assign_groups_similar_and_split_children() {
        let root = temp_root("auto");
        // 同一 repo・時間窓内・高類似 → 新規事案へ両所属
        let a = issue("k1", "me/r", "pages build failed", "queued");
        let b = issue("k2", "me/r", "pages build failed", "queued");
        store::save_issues(&root, &[a.clone(), b.clone()]);
        let id = auto_assign(&root, &b).unwrap();
        let incs = load_incidents(&root);
        assert_eq!(incs.len(), 1);
        assert_eq!(incs[0]["members"].as_array().unwrap().len(), 2);
        let issues = store::load_issues(&root);
        assert_eq!(str_of(&issues[0], "incident"), id);
        assert_eq!(str_of(&issues[1], "incident"), id);
        // splitFrom リンクは類似度なしでも同一事案へ
        let mut c = issue("k3", "me/r", "unrelated child", "queued");
        c["splitFrom"] = json!("k1");
        let mut issues2 = issues.clone();
        issues2.push(c.clone());
        store::save_issues(&root, &issues2);
        let id2 = auto_assign(&root, &c).unwrap();
        assert_eq!(id2, id);
        assert_eq!(
            load_incidents(&root)[0]["members"].as_array().unwrap().len(),
            3
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_assign_ignores_distant_or_closed_issues() {
        let root = temp_root("auto-solo");
        // 終端課題は関連先にならない
        let a = issue("k1", "me/r", "pages build failed", "done");
        let b = issue("k2", "me/r", "pages build failed", "queued");
        store::save_issues(&root, &[a, b.clone()]);
        assert!(auto_assign(&root, &b).is_none());
        // repo が違えば集約しない
        let c = issue("k3", "other/x", "pages build failed", "queued");
        let d = issue("k4", "me/r", "pages build failed", "queued");
        store::save_issues(&root, &[c, d.clone()]);
        assert!(auto_assign(&root, &d).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dispatchable_lists_queued_members() {
        let root = temp_root("disp");
        let issues = vec![
            issue("k1", "me/r", "a", "queued"),
            issue("k2", "me/r", "b", "running"),
            issue("k3", "me/r", "c", "done"),
        ];
        store::save_issues(&root, &issues);
        let inc = merge(&root, &["k1".into(), "k2".into(), "k3".into()], None).unwrap();
        let keys = dispatchable(&root, str_of(&inc, "id")).unwrap();
        assert_eq!(keys, vec!["k1".to_string()]);
        assert!(dispatchable(&root, "inc-none").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
