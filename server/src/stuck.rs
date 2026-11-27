// stuck.rs — Stuck Detector（run-control.mmd・manual-1a459b0c）
//
// OpenHands の Stuck Detector を参考に、実行体の (action, observation) 相当の
// 出力行履歴から非生産パターンを検出して run を halt する。
// -p 非対話実行では tool action を構造取得できないため、出力行の正規化
// （空白畳み＋数字を # へ丸め）を (action,observation) の代理として扱う。
//
// 検出パターン（OpenHands 準拠の行レベル写し）:
//   ① 同一 action-observation 4回以上   → 同一正規化行が直近窓内で4回
//   ② 同一 action-error 3回以上         → 同一 error 行が直近窓内で3回
//   ③ ユーザ入力なしの連続メッセージ    → 同一正規化行が3連続（独白相当）
//     （独白）3回以上
//   ④ 2パターンの交互繰返し 6回以上     → 直近6行が ABABAB
//   ⑤ context window エラー連続         → 2連続
//
// 検出結果は Detection として返し、呼び出し側（runner の監視ループ）が
// issue.lastStuck へ構造化記録して run を halt する。
use std::collections::VecDeque;

/// 検出結果（pattern は安定識別子。detail は人可読の代表行）
pub struct Detection {
    pub pattern: &'static str,
    pub detail: String,
}

/// 監視する直近行窓（OpenHands の履歴窓相当）
const WINDOW: usize = 24;

/// 空白畳みの正規化。`digits` 指定時は数字を # へ丸める。
/// 数字は error 行だけ丸める —— 進捗行（"step 1 done" → "step 2 done"）
/// をカウンタ差分で同一視すると正常進捗を誤検するため
fn normalize(line: &str, digits: bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut ws = false;
    for c in line.trim().chars() {
        if c.is_whitespace() {
            if !ws {
                out.push(' ');
            }
            ws = true;
        } else if digits && c.is_ascii_digit() {
            out.push('#');
            ws = false;
        } else {
            out.push(c);
            ws = false;
        }
    }
    out
}

/// error 行判定（stderr 行、または error 系の語を含む行）
fn is_err_line(stream: &str, line: &str) -> bool {
    if stream == "err" {
        return true;
    }
    let l = line.to_lowercase();
    ["error", "failed", "exception", "panic", "timeout", "fatal", "エラー"]
        .iter()
        .any(|k| l.contains(k))
}

/// context window エラー判定
fn is_ctx_err(line: &str) -> bool {
    let l = line.to_lowercase();
    ["context window", "context length", "context_length", "too many tokens",
     "maximum context", "token limit", "コンテキスト"]
        .iter()
        .any(|k| l.contains(k))
}

pub struct Detector {
    /// 直近 WINDOW 行の正規化行（newest last）
    lines: VecDeque<String>,
}

impl Detector {
    pub fn new() -> Self {
        Self {
            lines: VecDeque::with_capacity(WINDOW + 1),
        }
    }

    /// 1行を供給し、非生産パターンに達したら Detection を返す。
    /// 空行は供給しない（進捗無しの空白連打を誤検しない）
    pub fn feed(&mut self, stream: &str, line: &str) -> Option<Detection> {
        let err = is_err_line(stream, line);
        // error 行のみ数字を # へ丸める（同一 error のカウンタ揺れを
        // 同一視するため。進捗行の数字は残す）
        let n = normalize(line, err);
        if n.is_empty() {
            return None;
        }
        // ⑤ context window エラー連続（行そのもので判定。正規化前に見る）
        if is_ctx_err(line) {
            let prev_ctx = self
                .lines
                .back()
                .map(|p| p.starts_with("CTXERR|"))
                .unwrap_or(false);
            self.push(format!("CTXERR|{}", n));
            if prev_ctx {
                return Some(Detection {
                    pattern: "context_window",
                    detail: trim(&n),
                });
            }
            return None;
        }
        self.push(if err {
            format!("ERR|{}", n)
        } else {
            n.clone()
        });

        // ④ 2パターン交互 6行（ABABAB。直近6行が2値の交互列）
        if self.lines.len() >= 6 {
            let tail: Vec<&String> = self.lines.iter().rev().take(6).collect();
            let a = tail[0];
            let b = tail[1];
            if a != b
                && tail[2] == a
                && tail[3] == b
                && tail[4] == a
                && tail[5] == b
            {
                return Some(Detection {
                    pattern: "alternating",
                    detail: format!("{} / {}", trim(b), trim(a)),
                });
            }
        }
        // ② 同一 action-error 3回（ERR| 付き正規化行の窓内出現数。
        // 同一行3連続の独白より先に見る —— error 反復の方が具体的な症状）
        if err {
            let key = format!("ERR|{}", n);
            let cnt = self.lines.iter().filter(|l| **l == key).count();
            if cnt >= 3 {
                return Some(Detection {
                    pattern: "same_error",
                    detail: trim(&n),
                });
            }
        }
        // ③ 独白相当（同一行3連続）
        if self.lines.len() >= 3 {
            let tail: Vec<&String> = self.lines.iter().rev().take(3).collect();
            if tail[0] == tail[1] && tail[1] == tail[2] {
                return Some(Detection {
                    pattern: "monologue",
                    detail: trim(&n),
                });
            }
        }
        // ① 同一 action-observation 4回（正規化行の窓内出現数）
        {
            let cnt = self.lines.iter().filter(|l| *l == &n).count();
            if cnt >= 4 {
                return Some(Detection {
                    pattern: "same_action_obs",
                    detail: trim(&n),
                });
            }
        }
        None
    }

    fn push(&mut self, s: String) {
        if self.lines.len() >= WINDOW {
            self.lines.pop_front();
        }
        self.lines.push_back(s);
    }
}

fn trim(s: &str) -> String {
    s.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_same_action_observation_4x() {
        // 同一行4回（非連続。3連続は monologue として先に検出される）
        let mut d = Detector::new();
        assert!(d.feed("out", "Reading file src/main.rs").is_none());
        assert!(d.feed("out", "Reading file src/main.rs").is_none());
        assert!(d.feed("out", "writing patch").is_none());
        assert!(d.feed("out", "Reading file src/main.rs").is_none());
        let h = d.feed("out", "Reading file src/main.rs").unwrap();
        assert_eq!(h.pattern, "same_action_obs");
    }

    #[test]
    fn detects_same_error_3x() {
        let mut d = Detector::new();
        let mut hit = None;
        for i in 0..3 {
            hit = hit.or(d.feed("err", &format!("command failed rc={}", i)));
        }
        // 数字は # へ丸めるため rc=# で同一視
        assert_eq!(hit.unwrap().pattern, "same_error");
    }

    #[test]
    fn detects_monologue_3_consecutive() {
        let mut d = Detector::new();
        assert!(d.feed("out", "a").is_none());
        assert!(d.feed("out", "b").is_none());
        assert!(d.feed("out", "思考中…").is_none());
        assert!(d.feed("out", "思考中…").is_none());
        let h = d.feed("out", "思考中…").unwrap();
        assert_eq!(h.pattern, "monologue");
    }

    #[test]
    fn detects_alternating_2x6() {
        let mut d = Detector::new();
        let mut hit = None;
        for _ in 0..3 {
            hit = hit.or(d.feed("out", "run test"));
            hit = hit.or(d.feed("out", "edit file"));
        }
        assert_eq!(hit.unwrap().pattern, "alternating");
    }

    #[test]
    fn detects_context_window_2x() {
        let mut d = Detector::new();
        assert!(d
            .feed("err", "error: context window exceeded")
            .is_none());
        let h = d
            .feed("err", "context length limit reached again")
            .unwrap();
        assert_eq!(h.pattern, "context_window");
    }

    #[test]
    fn no_detection_on_normal_progress() {
        let mut d = Detector::new();
        for i in 0..40 {
            assert!(d.feed("out", &format!("step {} done", i)).is_none());
        }
    }
}
