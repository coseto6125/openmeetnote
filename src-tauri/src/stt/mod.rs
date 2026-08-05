//! 本機轉錄引擎（BLUEPRINT.md §5.3.1）。
//!
//! 兩個引擎，一快一準：Paraformer 供錄音期間的即時稿，whisper 供片段定稿。
//! 兩者對同一段的結果不一致時，該處是 `Gap`，交給使用者確認 —— 這不是備援，
//! 實測顯示兩個引擎的錯誤不重疊，分歧本身就指出最可能出錯的位置。
//!
//! 這個模組只負責「音訊進、帶時間片段出」。斷句規則、修訂語意與 `Gap` 要怎麼
//! 進事件日誌，都不在這裡 —— 那是 `Transcript` 的責任。

pub mod diff;
pub mod live;
pub mod paraformer;
pub mod speakers;
pub mod whisper;

/// 一段連續發言。時間以擷取音訊為基準（`captured_audio_ms`），不是會議時間軸：
/// 暫停期間沒有音訊，兩者會分岔，混用會讓引用定位到錯的地方。
// f32 沒有 Eq。Segment 只在測試裡比對，PartialEq 就夠。
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// whisper 判斷這段沒有語音的機率。
    ///
    /// 幻覺（憑空生出「字幕志願者 XXX」這類訓練資料殘留）幾乎都伴隨高值：
    /// 模型自己知道沒東西可轉，但解碼器仍被迫吐出 token。能量閘門擋不住
    /// 這種情況 —— 環境噪音的能量可以剛好高過門檻。
    pub no_speech: f32,
}

/// whisper 在沒有語音的音訊上會吐出的訓練資料殘留。
///
/// 模型的中文語料大量來自字幕網站，沒東西可轉時解碼器仍被迫產生 token，
/// 落點就是這些字幕組的署名與頻道口號。實測純靜音（全 0 樣本）可以穩定
/// 重現「中文字幕志愿者 杨栋梁」。
///
/// 本來想用 whisper 自己的 no_speech 機率來判斷，但實測 beam search 路徑下
/// 它一律回傳 0，連純靜音那段也是，那個訊號在這裡不存在。
const HALLUCINATIONS: &[&str] = &[
    "字幕志愿者",
    "字幕志願者",
    // 殘留被切成兩句時，第二句只剩署名。單看它像正常詞，但要整批每一句
    // 都命中才會丟，真實會議提到志願者的那一批一定還有別的句子。
    "志愿者",
    "志願者",
    "中文字幕",
    "字幕由",
    "amaraorg",
    "谢谢观看",
    "謝謝觀看",
    "点赞订阅",
    "點贊訂閱",
    "订阅我的频道",
    "訂閱我的頻道",
];

/// 用一個乾淨的 VAD 判斷這段音訊裡有多少毫秒是人聲。
///
/// 每次都建新的偵測器，這是給測試與量測用的入口；錄音路徑上的閘門沿用
/// 每軌一個的長駐實例，避免每批付模型載入的錢。
pub fn gate_voiced_ms(vad_model: &str, samples: &[f32]) -> Result<u64> {
    crate::stt::live::gate_voiced_ms(vad_model, samples)
}

/// 這一整批是不是憑空生出來的。
///
/// 判斷整批而不是單句：whisper 會把一段殘留切成好幾句（「中文字幕」+
/// 「志愿者 杨栋梁」），只看單句就會整批放行。反過來，真實會議裡有人說
/// 「謝謝觀看」的時候，那一批通常還有別的句子，那些句子不命中模式，
/// 整批就留下來。
///
/// 能量條件仍在：小聲說的話能量低，但它是真話，所以低能量只是必要條件
/// 不是充分條件，還要整批都命中已知殘留才丟。
///
/// 已知取捨：安靜的講者只說一句「謝謝觀看」就結束，會被誤判。文字比對
/// 無法證明來源，這一層留著是為了 VAD 放行後仍混進殘留的情況；VAD 上線
/// 之後實測七十分鐘它一次都沒觸發過。
pub fn is_hallucination(texts: &[&str], rms: f32) -> bool {
    const QUIET: f32 = 0.02;
    if texts.is_empty() {
        return false;
    }
    // 字幕組署名不套能量條件：它在任何音量下都是訓練資料殘留，不可能是
    // 真人說的話。實測外放的回音墊高了底噪（RMS 0.0326），讓「中文字幕
    // 志愿者 杨茜茜」直接跳過整個檢查混進逐字稿。
    if all_residue(texts) {
        return true;
    }
    // 結構判準留著能量條件：真人在正常音量下也可能重複強調或連說短句，
    // 那是真話。只有在幾乎沒有訊號的音訊上，這些形狀才代表解碼器空轉。
    if rms >= QUIET {
        return false;
    }
    runaway_repetition(texts) || fragment_burst(texts)
}

/// 整批每一句都命中已知的訓練資料殘留。
fn all_residue(texts: &[&str]) -> bool {
    texts.iter().all(|text| {
        let t: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
            .collect::<String>()
            .to_lowercase();
        HALLUCINATIONS.iter().any(|h| t.contains(h))
    })
}

/// 同一個詞連續重複到不像人話。
///
/// 解碼器在幾乎沒有訊號的音訊上會卡在同一個 token 出不來，實測轉出
/// 「好 等一下 等一下 等一下 等一下 等一下 等一下」。真人講話不會這樣：
/// 54 批真實會議音訊裡，句內連續重複最多是 1 次，也就是完全沒有重複過。
fn runaway_repetition(texts: &[&str]) -> bool {
    const LIMIT: usize = 4;
    texts.iter().any(|t| {
        let words: Vec<&str> = t.split_whitespace().collect();
        let mut run = 1;
        words.windows(2).any(|w| {
            run = if w[0] == w[1] { run + 1 } else { 1 };
            run >= LIMIT
        })
    })
}

/// 整批都是一兩個字的碎片。
///
/// 另一種解碼器空轉的形狀：實測轉出 ["2","3","4","4","5","6","6","6","7","7"]。
/// 真實會議不會連續五句都只有一兩個字，54 批裡一批都沒有。
fn fragment_burst(texts: &[&str]) -> bool {
    const MIN_SEGMENTS: usize = 5;
    texts.len() >= MIN_SEGMENTS
        && texts
            .iter()
            .all(|t| t.chars().filter(|c| !c.is_whitespace()).count() <= 2)
}

/// 單一 token 與它的時間點。
///
/// 比對階段用 token 而不是段：兩個引擎的斷句規則不同，先合併成段再按比例
/// 裁切會引入數個字的邊界誤差，把真正的用詞差異淹沒在切邊雜訊裡。
#[derive(Debug, Clone)]
pub struct Token {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("讀取音訊失敗：{0}")]
    Audio(String),
    #[error("模型載入失敗：{0}")]
    Load(String),
    #[error("轉錄失敗：{0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, SttError>;

/// 讀 16 kHz 單聲道 WAV。
///
/// 取樣率與聲道數不符時直接失敗而不是重新取樣：兩個引擎都只吃 16 kHz 單聲道，
/// 在這裡悄悄轉換會讓「為什麼結果變差」變得無從追查。
pub fn load_wav_16k_mono(path: &str) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path).map_err(|e| SttError::Audio(e.to_string()))?;
    let spec = r.spec();
    if spec.sample_rate != 16_000 || spec.channels != 1 {
        return Err(SttError::Audio(format!(
            "需要 16 kHz 單聲道，拿到 {} Hz {} 聲道",
            spec.sample_rate, spec.channels
        )));
    }
    Ok(r.samples::<i16>()
        .map(|s| s.unwrap_or(0) as f32 / 32768.0)
        .collect())
}

#[cfg(test)]
mod hallucination_tests {
    use super::is_hallucination;

    // 兩小時 soak 實際漏過的兩筆，RMS 都只比能量門檻高一點
    #[test]
    fn test_a_lone_subtitle_credit_in_quiet_audio_is_dropped() {
        assert!(is_hallucination(&["字幕志愿者 杨茜茜"], 0.0051));
        assert!(is_hallucination(&["中文字幕志愿者 杨栋梁"], 0.0067));
    }

    /// whisper 會把一段殘留切成好幾句，只看第一句就會整批放行
    #[test]
    fn test_a_credit_split_across_segments_is_still_dropped() {
        assert!(is_hallucination(&["中文字幕", "志愿者 杨栋梁"], 0.0051));
    }

    #[test]
    fn test_the_same_words_are_kept_when_the_batch_has_other_sentences() {
        // 有人真的在講「謝謝觀看」時，那批通常還有別的句子
        assert!(!is_hallucination(
            &["謝謝觀看", "我們下次會議再討論", "散會"],
            0.0051
        ));
    }

    #[test]
    fn test_a_quiet_real_utterance_is_kept() {
        // 小聲說的短句能量同樣很低，但不能刪
        assert!(!is_hallucination(&["好"], 0.0061));
        assert!(!is_hallucination(&["我們通過"], 0.004));
    }

    #[test]
    fn test_punctuation_and_case_do_not_defeat_the_match() {
        assert!(is_hallucination(&["字幕 由 Amara.org 社群提供"], 0.003));
        // 正規化把點號拿掉了，模式也必須是拿掉之後的樣子
        assert!(is_hallucination(&["Subtitles by Amara.org"], 0.003));
    }

    #[test]
    fn test_an_empty_batch_is_not_a_hallucination() {
        assert!(!is_hallucination(&[], 0.001));
    }

    // 真機實測漏過的兩批，VAD 判有微弱人聲但 whisper 仍在上面編
    #[test]
    fn test_a_word_repeated_past_all_reason_is_dropped() {
        assert!(is_hallucination(
            &["好 等一下 等一下 等一下 等一下 等一下 等一下"],
            0.0095
        ));
    }

    #[test]
    fn test_a_burst_of_one_character_fragments_is_dropped() {
        assert!(is_hallucination(
            &["2", "3", "4", "4", "5", "6", "6", "6", "7", "7"],
            0.0149
        ));
    }

    #[test]
    fn test_natural_emphasis_is_not_repetition() {
        // 真人強調時會連說兩三次，那是真話
        assert!(!is_hallucination(&["等一下 等一下 我補充一點"], 0.0095));
        assert!(!is_hallucination(&["對 對 對"], 0.0095));
    }

    #[test]
    fn test_a_few_short_answers_are_not_a_fragment_burst() {
        // 四句以內的短應答留著，真實會議會這樣
        assert!(!is_hallucination(&["好", "對", "是"], 0.0095));
    }

    /// 真機錄下的會議批次，逐字照抄。判準再嚴也不能動到這些。
    #[test]
    fn test_real_meeting_batches_survive_every_rule() {
        for batch in [
            &[
                "賴斯堡委員所提主決議",
                "請文化部表來意見",
                "遵照辦理",
                "好 我們通過",
            ][..],
            &["嘿嘿 谢谢", "好", "哎哟"][..],
            &["麻煩吳主委請就座", "國客會請就座"][..],
            &["進入224改組決議", "225案改組決議", "226案改組決議"][..],
            &["你看 今天又在施工", "光是外面的窗戶"][..],
        ] {
            // 連最安靜的批次都要留住
            assert!(!is_hallucination(batch, 0.0095), "真實批次被丟：{batch:?}");
        }
    }

    /// 結構判準在正常音量下不作用：真人會重複強調，也會連說短句
    #[test]
    fn test_loud_audio_bypasses_the_structural_rules() {
        assert!(!is_hallucination(&["2", "3", "4", "4", "5", "6"], 0.06));
        assert!(!is_hallucination(&["好 好 好 好 好 好"], 0.06));
    }

    /// 但字幕組署名不吃這一套。
    ///
    /// 實測外放的回音把底噪墊到 RMS 0.0326，讓這句跳過整個檢查混進逐字稿。
    #[test]
    fn test_a_subtitle_credit_is_caught_at_any_volume() {
        assert!(is_hallucination(&["中文字幕志愿者 杨茜茜"], 0.0326));
        assert!(is_hallucination(&["中文字幕志愿者 杨茜茜"], 0.5));
    }
}
