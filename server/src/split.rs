// split.rs — 並列モード（execMode=parallel）の課題分割（split.mjs 移植・純粋ロジック）
//
// parallel chat の初回 prompt には PARALLEL_PREAMBLE が前置され、
// エージェントは独立タスクへ分割できると判断した時点で
//   ```kasugai-split
//   [{"title":"子タスク名","prompt":"子 chat へ送る自己完結の指示"}]
//   ```
// を出力する。control はターン終了ごとに新着 agent メッセージへ
// extract_splits を掛け、見つかった要素を子 chat として自動起動する。
use regex::Regex;
use std::sync::OnceLock;

// 初回 prompt の先頭へ付ける分割指示。子 prompt は文脈を引き継がない
// 独立セッションで実行されるため、自己完結で書くよう指示する
pub const PARALLEL_PREAMBLE: &str = r#"[処理モード: 並列]
この指示が複数の独立した作業へ分割できると判断した場合、作業中・回答末尾を問わず
任意のタイミングで次の形式のコードブロックを出力してください:

```kasugai-split
[{"title":"短い子タスク名","prompt":"子チャットへ送る指示文"}]
```

各 prompt はこの会話の文脈を引き継がない独立チャット（別セッション）で実行されます。
必要な背景・対象ファイル・期待する成果物を prompt 内に自己完結で書いてください。
title・prompt とも日本語可。分割が不要と判断した場合はブロックを出さず、
そのまま作業を進めてください。ブロックを出した後も自分の作業は継続して構いません
（分割結果は子チャットの完了時にこのチャットへ報告されます）。"#;

#[derive(Debug, Clone)]
pub struct Split {
    pub title: String,
    pub prompt: String,
}

fn split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"```kasugai-split[^\S\r\n]*\r?\n([\s\S]*?)```").unwrap()
    })
}

/// テキスト中の ```kasugai-split ブロックから [{title,prompt}] を抽出する。
/// 壊れた JSON・必須フィールド欠落は捨てる（パース不能で処理を止めない）
pub fn extract_splits(text: &str) -> Vec<Split> {
    let mut out = Vec::new();
    for m in split_re().captures_iter(text) {
        let arr: serde_json::Value = match serde_json::from_str(&m[1]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(items) = arr.as_array() else { continue };
        for e in items {
            let title = e
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .chars()
                .take(80)
                .collect::<String>();
            let prompt = e
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .chars()
                .take(4000)
                .collect::<String>();
            if !title.is_empty() && !prompt.is_empty() {
                out.push(Split { title, prompt });
            }
        }
    }
    out
}

/// 重複抑止キー（title+prompt 一致）。chats.json の seenSplits へ永続化される
pub fn split_key(s: &Split) -> String {
    format!("{}\n{}", s.title.to_lowercase(), s.prompt.to_lowercase())
}
