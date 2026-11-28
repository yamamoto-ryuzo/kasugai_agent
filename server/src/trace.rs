// trace.rs — OTel GenAI 風構造化 span 記録（trace-waterfall.mmd・README §5.9）
//
// OpenTelemetry GenAI semantic conventions / OpenInference の span 仕様を
// 参考に、run の各段階（plan → exec → verify → push）を span として
// evidence/<slug>/run-<idx>/trace.jsonl へ1行1spanで追記する。
// span は完了時に1行で書き出す（JSONL 追記で中断耐性）。瞬時イベントは
// durationMs=0 の点 span。trajectory（逐語記録）とは別物で、こちらは
// 構造化された工程の骨格だけを持つ。
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::trajectory::run_dir;
use crate::util::now_iso;

/// span.kind（OTel GenAI 風）。run 全体=AGENT、実行体呼出=LLM、
/// verify=EVALUATOR、git clean/push=TOOL、その他=CHAIN
pub const KIND_AGENT: &str = "AGENT";
pub const KIND_LLM: &str = "LLM";
pub const KIND_EVALUATOR: &str = "EVALUATOR";
pub const KIND_TOOL: &str = "TOOL";

fn millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn span_id() -> String {
    crate::util::rand_hex(8)
}

struct Open {
    name: String,
    kind: String,
    parent: Option<String>,
    start_ms: i64,
    start_iso: String,
    attrs: Map<String, Value>,
}

/// run 単位の span 追記器。begin で traceId を発行し、start/end で
/// span を閉じて trace.jsonl へ追記する。共有は Arc<Trace>
pub struct Trace {
    path: PathBuf,
    trace_id: String,
    root_id: String,
    open: Mutex<HashMap<String, Open>>,
}

impl Trace {
    /// dir = evidence/<slug>/run-<idx>/。root span（kasugai.run・AGENT）を
    /// 開いた状態で返す。trace_id は run ごとに発行（揮発 id でよい）
    pub fn begin(dir: PathBuf) -> std::sync::Arc<Self> {
        let _ = std::fs::create_dir_all(&dir);
        let t = std::sync::Arc::new(Self {
            path: dir.join("trace.jsonl"),
            trace_id: crate::util::rand_hex(16),
            root_id: span_id(),
            open: Mutex::new(HashMap::new()),
        });
        t.start_named(&t.root_id.clone(), "kasugai.run", KIND_AGENT, None, Map::new());
        t
    }

    /// 既存 run dir の trace を開く（verify 再開等の別タスクから追記する
    /// 用途。traceId は trace.jsonl の既存 span から復元し、無ければ新規）
    pub fn open(dir: PathBuf) -> std::sync::Arc<Self> {
        let path = dir.join("trace.jsonl");
        let mut trace_id = crate::util::rand_hex(16);
        let mut root_id = span_id();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for l in text.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(l) {
                    if let Some(t) = v.get("traceId").and_then(|x| x.as_str()) {
                        trace_id = t.to_string();
                    }
                    if v.get("parentSpanId").is_none()
                        || v.get("parentSpanId").map(|x| x.is_null()).unwrap_or(false)
                    {
                        if let Some(s) = v.get("spanId").and_then(|x| x.as_str()) {
                            root_id = s.to_string();
                        }
                    }
                    break;
                }
            }
        }
        let t = std::sync::Arc::new(Self {
            path,
            trace_id,
            root_id: root_id.clone(),
            open: Mutex::new(HashMap::new()),
        });
        // root span が未完了なら再オープン状態に載せる（end(root) で閉じる）
        t.start_named(&root_id, "kasugai.run", KIND_AGENT, None, Map::new());
        t
    }

    /// 子 span を開く（parent 省略時は root span の子）。戻り値は spanId
    pub fn start(&self, name: &str, kind: &str, attrs: Map<String, Value>) -> String {
        let id = span_id();
        self.start_named(&id, name, kind, Some(self.root_id.clone()), attrs);
        id
    }

    fn start_named(
        &self,
        id: &str,
        name: &str,
        kind: &str,
        parent: Option<String>,
        attrs: Map<String, Value>,
    ) {
        self.open.lock().unwrap().insert(
            id.to_string(),
            Open {
                name: name.to_string(),
                kind: kind.to_string(),
                parent,
                start_ms: millis(),
                start_iso: now_iso(),
                attrs,
            },
        );
    }

    /// span を閉じて trace.jsonl へ1行追記。status は ok→OK / それ以外→ERROR。
    /// extra_attrs は gen_ai.usage.* 等の終了時に確定する属性
    pub fn end(&self, id: &str, ok: bool, message: &str, extra_attrs: Map<String, Value>) {
        let Some(o) = self.open.lock().unwrap().remove(id) else {
            return;
        };
        let end_ms = millis();
        let mut attrs = o.attrs;
        for (k, v) in extra_attrs {
            attrs.insert(k, v);
        }
        self.write_span(json!({
            "traceId": self.trace_id,
            "spanId": id,
            "parentSpanId": o.parent,
            "name": o.name,
            "kind": o.kind,
            "startTime": o.start_iso,
            "endTime": now_iso(),
            "durationMs": end_ms - o.start_ms,
            "status": {"code": if ok { "OK" } else { "ERROR" }, "message": message},
            "attributes": Value::Object(attrs),
        }));
    }

    /// 瞬時イベントを durationMs=0 の点 span として記録（状態遷移・
    /// 差し戻し等。ウォーターフォール上では縦線として描かれる）
    pub fn event(&self, name: &str, attrs: Map<String, Value>) {
        self.write_span(json!({
            "traceId": self.trace_id,
            "spanId": span_id(),
            "parentSpanId": self.root_id,
            "name": name,
            "kind": "EVENT",
            "startTime": now_iso(),
            "endTime": now_iso(),
            "durationMs": 0,
            "status": {"code": "OK", "message": ""},
            "attributes": Value::Object(attrs),
        }));
    }

    /// root span を閉じる（run 終了時）
    pub fn finish_root(&self, ok: bool, message: &str) {
        self.end(&self.root_id.clone(), ok, message, Map::new());
    }

    fn write_span(&self, span: Value) {
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            // secret redaction（manual-a3a60c51）: attributes/message は
            // verify 理由等の出力由来文字列を含み得るため書き込み点でマスク
            let _ = writeln!(f, "{}", crate::redact::scrub(&span.to_string()));
        }
    }
}

/// 課題の run-<idx>/trace.jsonl を読む（API 用。古い run でも読めるよう
/// 永続化データのみに依存する）
pub fn read_spans(root: &Path, key: &str, idx: usize) -> Vec<Value> {
    let path = run_dir(
        &root.join("evidence").join(crate::knowledge::key_slug(key)),
        idx,
    )
    .join("trace.jsonl");
    let mut spans = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for l in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                spans.push(v);
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-trace-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn spans_are_parented_and_timed() {
        let dir = temp_root("io").join("run-0");
        let t = Trace::begin(dir.clone());
        let s = t.start("llm.exec", KIND_LLM, Map::new());
        t.end(&s, true, "", Map::new());
        t.event("state.verifying", Map::new());
        t.finish_root(true, "done");
        let text = std::fs::read_to_string(dir.join("trace.jsonl")).unwrap();
        let spans: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0]["name"], json!("llm.exec"));
        assert_eq!(spans[0]["kind"], json!("LLM"));
        assert_eq!(spans[0]["status"]["code"], json!("OK"));
        assert_eq!(spans[1]["name"], json!("state.verifying"));
        assert_eq!(spans[1]["durationMs"], json!(0));
        assert_eq!(spans[2]["name"], json!("kasugai.run"));
        // 全 span が同一 traceId で root 以外は root を親に持つ
        let tid = spans[0]["traceId"].clone();
        assert!(spans.iter().all(|s| s["traceId"] == tid));
        assert!(spans[0]["parentSpanId"].is_string());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn open_restores_trace_id() {
        let dir = temp_root("reopen").join("run-1");
        {
            let t = Trace::begin(dir.clone());
            let s = t.start("llm.exec", KIND_LLM, Map::new());
            t.end(&s, true, "", Map::new());
        }
        let tid = {
            let text = std::fs::read_to_string(dir.join("trace.jsonl")).unwrap();
            let v: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
            v["traceId"].as_str().unwrap().to_string()
        };
        let t = Trace::open(dir.clone());
        t.event("state.done", Map::new());
        let text = std::fs::read_to_string(dir.join("trace.jsonl")).unwrap();
        let last: Value =
            serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["traceId"], json!(tid));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }
}
