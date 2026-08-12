//! 兩個引擎的結果比對，產出待確認位置。
//!
//! 這裡不判斷誰對誰錯，只指出兩者不同。實測兩個引擎的錯誤不重疊（whisper 錯
//! 「拼板舟」時 Paraformer 對，Paraformer 錯「達悟族」時 whisper 對），因此
//! 分歧位置就是最值得使用者看一眼的位置，而猜哪一邊對只會製造假的確定性。

use super::{Segment, Token};

/// 詞彙校正。
///
/// 存在的理由有兩個，形狀相同所以合成一張表：
///
/// 1. zhconv 的台灣轉換不含部分 IT 詞彙（「網絡」該是「網路」）。
/// 2. 專有名詞是所有引擎的共同盲區，而它是事後校正。為什麼不改餵給模型當
///    提示，理由與量到的數字記在 `from_file` 上。
pub struct Corrections(Vec<(String, String)>);

/// 詞彙轉換的內建項。使用者的專有名詞另外附加，不寫死在這裡。
const BUILTIN: &[(&str, &str)] = &[("網絡", "網路"), ("信息", "資訊"), ("軟件", "軟體")];

impl Default for Corrections {
    fn default() -> Self {
        Self(
            BUILTIN
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
        )
    }
}

impl Corrections {
    /// 從詞表檔載入。每行一組 `錯誤=正確`，`#` 開頭是註解。
    ///
    /// 專有名詞是所有轉錄引擎的共同盲區（實測「召委」「西拉雅」「雙橡園」
    /// 沒有任何模型轉得對），而每個人的會議裡固定出現的人名、機關名、產品
    /// 代號都不一樣，內建詞表不可能涵蓋。這裡讓使用者自己補。
    ///
    /// 事後校正而不是餵給模型當提示。理由是量出來的，數字與重跑方式見
    /// `stt::whisper` 的 `initial_prompt_probe`。同一段會議音訊、七個關鍵詞，
    /// 提示裡塞幾個詞對上幾個：
    ///
    /// | 提示 | 命中 |
    /// |---|---|
    /// | 不帶 | 3 |
    /// | 7 詞（全是這場講到的） | 5 |
    /// | 15 詞 | 3 |
    /// | 22 詞 | 3 |
    /// | 37 詞 | 2 |
    ///
    /// 好處只在「提示裡全是這場真的會講到的詞」時存在，加八個沒講到的詞就
    /// 沒了，而且那八個詞還會把本來就對的「達悟族」「原民會」弄錯。詞表的
    /// 用途正是收那些「哪場會用得到還不知道」的詞，所以做不到那個前提，也
    /// 沒有一個安全的長度上限可以設。
    ///
    /// 「召委」與「拼板舟」則是每一種長度都轉不對，校正表一行就解決。提示
    /// 能做的事校正表都能做，反過來不成立。
    ///
    /// （這裡原本寫的是「initial prompt 會讓它整段跳過內容」。那個說法沒有
    /// 重現：probe 逐十秒統計字數，五種設定都沒有出現塌掉的區間。留著錯的
    /// 理由會讓下一個人用錯的判準重做這個決定。）
    pub fn from_file(path: &std::path::Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let pairs: Vec<(String, String)> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(from, to)| (from.trim().to_owned(), to.trim().to_owned()))
            .filter(|(from, _)| !from.is_empty())
            .collect();
        let mut out = Self::default();
        out.0 = pairs.into_iter().chain(out.0).collect();
        out
    }

    /// 附加使用者詞彙。後加的先套用，讓專案詞表能覆蓋內建項。
    pub fn with(mut self, pairs: &[(&str, &str)]) -> Self {
        let mut extra: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect();
        extra.append(&mut self.0);
        Self(extra)
    }

    pub fn apply(&self, s: &str) -> String {
        self.0
            .iter()
            .fold(s.to_owned(), |acc, (from, to)| acc.replace(from, to))
    }
}

/// 轉為繁體並套用校正。輸出是要給使用者看的文字。
pub fn to_traditional(s: &str, corrections: &Corrections) -> String {
    corrections.apply(&zhconv::zhconv(s, zhconv::Variant::ZhTW))
}

/// 比對用的正規化：去掉標點與語氣詞。
///
/// 兩個引擎必然在這兩者上不同（Paraformer 保留「呃」「哈」，whisper 清掉），
/// 那是風格不是分歧。先消掉，剩下的差異才值得使用者花時間看。
fn for_compare(s: &str, corrections: &Corrections) -> String {
    to_traditional(s, corrections)
        .chars()
        .filter(|c| !"，。、？！：；「」…,.?!: 　".contains(*c) && !"呃哈啊喔哦嗯".contains(*c))
        .collect()
}

/// 落在片段時間範圍內的 token 文字。
///
/// 兩端各放寬 `pad_ms`：兩個引擎對同一個字的時間點本來就會差一點，抓太緊會
/// 把邊界的字切掉，反而製造假分歧。
fn tokens_within(seg: &Segment, toks: &[Token], pad_ms: u64) -> String {
    toks.iter()
        .filter(|t| t.at_ms + pad_ms >= seg.start_ms && t.at_ms <= seg.end_ms + pad_ms)
        .map(|t| t.text.as_str())
        .collect()
}

#[derive(Debug, Clone)]
pub struct Comparison {
    pub segment: Segment,
    /// 同一時間範圍內另一個引擎的文字
    pub counterpart: String,
    /// 0.0 至 1.0，正規化後的字元相似度
    pub similarity: f64,
    pub agrees: bool,
}

/// 對造文字是按時間切出來的，邊界必然差幾個字，因此用相似度而非全等判斷。
/// 0.75 是實測值：再高會把切邊誤差報成分歧，再低會漏掉真正的用詞差異。
pub const AGREEMENT_THRESHOLD: f64 = 0.75;

pub fn compare(
    reference: &[Segment],
    tokens: &[Token],
    corrections: &Corrections,
    pad_ms: u64,
) -> Vec<Comparison> {
    reference
        .iter()
        .map(|seg| {
            let counterpart = tokens_within(seg, tokens, pad_ms);
            let (a, b) = (
                for_compare(&seg.text, corrections),
                for_compare(&counterpart, corrections),
            );
            let similarity = if a.is_empty() {
                0.0
            } else {
                strsim::normalized_levenshtein(&a, &b)
            };
            Comparison {
                segment: seg.clone(),
                counterpart: to_traditional(&counterpart, corrections),
                similarity,
                agrees: similarity >= AGREEMENT_THRESHOLD,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: u64, end_ms: u64, text: &str) -> Segment {
        Segment {
            no_speech: 0.0,
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    fn toks(items: &[(u64, &str)]) -> Vec<Token> {
        items
            .iter()
            .map(|(at_ms, text)| Token {
                at_ms: *at_ms,
                text: (*text).to_string(),
            })
            .collect()
    }

    #[test]
    fn test_simplified_output_is_converted_before_comparing() {
        // Paraformer 輸出簡體，whisper 輸出繁體。沒轉換的話每一段都會是分歧
        let r = compare(
            &[seg(0, 1000, "原住民基本法")],
            &toks(&[(200, "原住民"), (600, "基本法")]),
            &Corrections::default(),
            200,
        );
        assert!(r[0].agrees, "簡繁差異被誤判成分歧");
    }

    #[test]
    fn test_filler_words_do_not_count_as_disagreement() {
        // Paraformer 保留語氣詞，whisper 清掉；那是風格不是內容差異
        let r = compare(
            &[seg(0, 1000, "今天感謝召委")],
            &toks(&[(100, "呃"), (300, "今天"), (500, "呃"), (700, "感謝召委")]),
            &Corrections::default(),
            200,
        );
        assert!(r[0].agrees, "語氣詞被誤判成分歧");
    }

    #[test]
    fn test_a_real_word_difference_is_reported() {
        // 實際案例：whisper 轉「平板舟」，Paraformer 轉「拼板舟」，後者正確
        let r = compare(
            &[seg(0, 1000, "平板舟")],
            &toks(&[(200, "拼"), (500, "板舟")]),
            &Corrections::default(),
            200,
        );
        assert!(!r[0].agrees, "真正的用詞差異被吞掉了");
    }

    #[test]
    fn test_tokens_outside_the_window_are_excluded() {
        let r = compare(
            &[seg(1000, 2000, "海委會")],
            &toks(&[(100, "文化部"), (1500, "海委會"), (5000, "專案報告")]),
            &Corrections::default(),
            200,
        );
        assert_eq!(r[0].counterpart, "海委會");
    }

    #[test]
    fn test_a_vocabulary_file_is_applied_before_the_builtin_table() {
        let dir = std::env::temp_dir().join("omn-vocab-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vocabulary.txt");
        std::fs::write(&path, "# 會議詞表\n招委 = 召委\n希臘雅=西拉雅\n\n壞行=\n").unwrap();
        let c = Corrections::from_file(&path);
        assert_eq!(c.apply("感謝招委排審"), "感謝召委排審");
        assert_eq!(c.apply("希臘雅的證明"), "西拉雅的證明");
        // 內建的簡繁詞彙仍在
        assert_eq!(c.apply("網絡"), "網路");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_a_missing_vocabulary_file_falls_back_to_the_builtin_table() {
        // 沒有詞表是常態，不該讓轉錄失敗
        let c = Corrections::from_file(std::path::Path::new("/no/such/vocabulary.txt"));
        assert_eq!(c.apply("網絡"), "網路");
    }

    #[test]
    fn test_user_corrections_override_the_builtin_table() {
        let c = Corrections::default().with(&[("希臘雅", "西拉雅")]);
        assert_eq!(c.apply("希臘雅正名"), "西拉雅正名");
        // 內建項仍在
        assert_eq!(c.apply("網絡"), "網路");
    }

    #[test]
    fn test_an_empty_segment_never_counts_as_agreement() {
        // 空片段與任何東西的相似度都沒有意義，預設不可視為一致
        let r = compare(&[seg(0, 100, "")], &toks(&[]), &Corrections::default(), 200);
        assert!(!r[0].agrees);
    }
}
