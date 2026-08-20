//! whisper.cpp 定稿引擎。
//!
//! 實測 large-v3-turbo-q5 在 12 個台灣會議關鍵詞上命中 9 個，是測過的本機方案裡
//! 最準的（比較與已排除選項見 BLUEPRINT.md §5.3.1）。

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::{Result, Segment, SttError};

pub struct Whisper {
    ctx: WhisperContext,
    threads: i32,
}

impl Whisper {
    pub fn load(model_path: &str, threads: i32) -> Result<Self> {
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
            .map_err(|e| SttError::Load(e.to_string()))?;
        Ok(Self { ctx, threads })
    }

    /// 轉錄整段音訊，回傳帶時間區間的片段。
    ///
    /// 一律帶時間戳。`set_no_timestamps(true)` 會抑制時間戳 token 並改變解碼路徑，
    /// 微調模型在該模式下會提前輸出 EOT 而截斷（實測 Belle-zh 少掉三分之一內容）；
    /// 何況引用驗證本來就需要時間戳定位，關掉它沒有任何好處。
    pub fn transcribe(&self, samples: &[f32]) -> Result<Vec<Segment>> {
        self.run(samples, None)
    }

    /// `prompt` 是 whisper 的 initial prompt。出貨路徑一律傳 `None`，量到的
    /// 數字記在 `initial_prompt_probe`。留著這個參數是為了那個 probe 隨時能
    /// 重跑：上一次「實測不行」只留下一句結論，沒有留下重現的路徑，於是每隔
    /// 一陣子就有人想再試一次。
    fn run(&self, samples: &[f32], prompt: Option<&str>) -> Result<Vec<Segment>> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| SttError::Decode(e.to_string()))?;

        // 與 whisper.cpp CLI 的預設一致。greedy best_of=1 會明顯掉品質：
        // 同一個模型、同一段音訊，greedy 轉出「達物族」，beam search 轉出「達悟族」。
        let mut p = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        p.set_n_threads(self.threads);
        p.set_language(Some("zh"));
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        if let Some(t) = prompt {
            p.set_initial_prompt(t);
        }
        state
            .full(p, samples)
            .map_err(|e| SttError::Decode(e.to_string()))?;

        (0..state.full_n_segments())
            .map(|i| {
                let seg = state
                    .get_segment(i)
                    .ok_or_else(|| SttError::Decode(format!("取不到第 {i} 段")))?;
                Ok(Segment {
                    // whisper 的時間單位是 centisecond
                    start_ms: seg.start_timestamp() as u64 * 10,
                    end_ms: seg.end_timestamp() as u64 * 10,
                    no_speech: seg.no_speech_probability(),
                    // lossy 而不是嚴格解碼：whisper 偶爾會在段落邊界切斷一個
                    // 多位元組字元，嚴格版對整批回 Err，那一批的定稿就整個
                    // 消失、只剩即時稿。壞掉的那一個字變成 U+FFFD 是小得多的
                    // 損失。整場 52 檔的評測實測踩到 7 次。
                    text: seg
                        .to_str_lossy()
                        .map_err(|e| SttError::Decode(e.to_string()))?
                        .trim()
                        .to_owned(),
                })
            })
            .collect()
    }
}

/// 定稿引擎要不要吃使用者詞表當 initial prompt。
///
/// 答案是不要，而這個 module 存在是為了那句話有數字撐著。專有名詞是所有引擎
/// 的共同盲區，「把詞表餵給模型」看起來永遠比「事後校正」高明，所以這個念頭
/// 會反覆回來；先前的結論只寫在註解裡，沒有人能重跑。
#[cfg(test)]
mod initial_prompt_probe {
    use super::*;

    /// 提示詞就是使用者詞表右邊那一欄（README 的範例），near.wav 這場會議
    /// 裡真的會出現的專有名詞。
    const TERMS: &str = "召委、西拉雅、雙橡園、拼板舟、達悟族、原民會、排審";

    /// 密度統計的時間桶。短到看得出「這一段沒轉」，長到不會被一句話跨界誤判。
    const BUCKET_S: usize = 10;

    /// 詞表會長。一個跑了幾個月的專案詞表是幾十個詞，不是七個，而 whisper 的
    /// initial prompt 上限是 224 個 token —— 塞滿它跟只放七個詞是兩件事。
    ///
    /// `extra` 是在七個關鍵詞之外再塞幾個「這場會議沒講到、但詞表裡會有」的
    /// 詞。掃過幾個長度才知道效果是在哪裡翻掉的，只比最短與最長會把「有沒有
    /// 一個安全的上限」這個問題留著。
    fn prompt_of(extra: usize) -> String {
        let filler = [
            "原住民族委員會",
            "行政院",
            "立法院",
            "內政委員會",
            "預算凍結",
            "決議事項",
            "書面報告",
            "地方創生",
            "族語推廣",
            "文化健康站",
            "部落會議",
            "傳統領域",
            "自然主權",
            "諮商同意",
            "土地劃設",
            "身分認定",
            "族別註記",
            "平埔族群",
            "正名運動",
            "轉型正義",
            "礦業法",
            "採礦權",
            "環境影響評估",
            "水資源",
            "國土計畫",
            "都市原住民",
            "就業服務",
            "職業訓練",
            "長期照顧",
            "醫療給付",
        ];
        if extra == 0 {
            return TERMS.to_owned();
        }
        format!("{TERMS}、{}", filler[..extra].join("、"))
    }

    /// 每個時間桶轉出幾個字。片段可能跨桶，一律算在起點所在的桶。
    fn density(segs: &[Segment], buckets: usize) -> Vec<usize> {
        let mut out = vec![0; buckets];
        for s in segs {
            let i = (s.start_ms as usize / 1_000 / BUCKET_S).min(buckets - 1);
            out[i] += s.text.chars().count();
        }
        out
    }

    fn material() -> Option<(String, Vec<f32>)> {
        let model = std::env::var("OMN_WHISPER_MODEL").unwrap_or_else(|_| {
            "/home/enor/whisper-bench/models/ggml-large-v3-turbo-q5_0.bin".into()
        });
        let wav = "/home/enor/whisper-bench/near.wav";
        if !std::path::Path::new(&model).exists() || !std::path::Path::new(wav).exists() {
            return None;
        }
        Some((model, crate::stt::load_wav_16k_mono(wav).expect("讀音訊")))
    }

    /// 上面那支用 near.wav —— 立法院質詢、全中文、專有名詞是機關與法案名。
    /// 這個產品實際錄的會議在談主播分潤、Google Workspace、報價單，中英夾雜。
    /// 同一個結論不保證跨得過去：streaming zipformer-ctc 就是在 near.wav 上
    /// 贏過現行引擎、在真實會議上輸，因為它不會英文（BLUEPRINT §5.3 的排除表）。
    ///
    /// 所以同一個問題在自己的語料上再問一次。
    ///
    /// 目標詞是從那場會議的定稿逐字稿裡撈出來、確認真的出現過的 17 個。
    /// 前文分四種：不帶、只放講到的、使用者詞表、詞表加業務名詞。第三與第四
    /// 種是真實情境 —— 詞表會累積別場會議的詞，而使用者不會為每場會議改它。
    // 一詞一行是 rustfmt 的預設，17 個詞排成 17 行反而看不出這是一份清單
    #[rustfmt::skip]
    const OURS: &[&str] = &[
        "經紀人", "主播", "帳號", "管理員", "簽約", "分頁", "匯入", "趨勢圖",
        "建檔", "群組", "薪資", "後台", "會議記錄", "統整", "匯出", "抽成",
        "單獨窗口",
    ];

    /// 使用者詞表右邊那一欄。九個裡有五個是別場會議的。
    const OUR_VOCAB: &str = "召委、西拉雅、雙橡園、拼板舟、達悟族、未稅、含稅、報價單、溢價";

    /// 詞表之外還會進前文的業務名詞。這場沒講到的（報價單、權限管理、行事曆）
    /// 刻意留著：詞表就是拿來收「哪場會用到還不知道」的詞。
    const OUR_NOUNS: &str = "主播、經紀人、建檔、匯入、匯出、報價單、權限管理、行事曆、簽約、抽成、薪資、後台、帳號、群組、趨勢圖、單獨窗口、會議記錄";

    /// 批次長度。整段送進去的話 whisper 會在尾端靜音上空轉出重複迴圈，那是
    /// 批次長度的問題不是前文的問題，混進數字裡會蓋掉要量的差異。線上也是
    /// 分批餵，前文每批都會重新套用一次。
    const OURS_BATCH_S: usize = 28;

    fn our_material() -> Option<(String, Vec<f32>)> {
        let model = std::env::var("OMN_WHISPER_MODEL").unwrap_or_else(|_| {
            "/home/enor/whisper-bench/models/ggml-large-v3-turbo-q5_0.bin".into()
        });
        let dir = std::env::var("OMN_PROBE_MEETING").ok()?;
        if !std::path::Path::new(&model).exists() {
            return None;
        }
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().is_some_and(|x| x == "wav")
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("mic-"))
            })
            .collect();
        files.sort();
        if files.is_empty() {
            return None;
        }
        let mut samples = Vec::new();
        for f in &files {
            samples.extend(crate::stt::load_wav_16k_mono(f.to_str()?).ok()?);
        }
        Some((model, samples))
    }

    /// 同一個問題，換成自己的會議語料。
    ///
    /// 上一次量這件事得到「前文有用」的相反結論，破綻在對照組不對等：兩趟
    /// 丟掉的批次數不同（16 對 0），多出來的目標詞來自多處理的那些音訊，
    /// 不是辨識變準。所以這裡不放閘門，每個設定看到的批次完全相同。
    #[test]
    #[ignore = "需要真實會議錄音，設 OMN_PROBE_MEETING 指向 mic-*.wav 的目錄"]
    fn probe_whether_an_initial_prompt_helps_our_own_meetings() {
        let Some((model, samples)) = our_material() else {
            eprintln!("略過：缺少模型或 OMN_PROBE_MEETING");
            return;
        };
        let audio_s = samples.len() / 16_000;
        eprintln!("音訊 {} 分 {} 秒", audio_s / 60, audio_s % 60);

        let vocab_plus = format!("{OUR_VOCAB}、{OUR_NOUNS}。");
        let ideal = format!("{}。", OURS.join("、"));
        let vocab_only = format!("{OUR_VOCAB}。");
        let configs: [(&str, Option<&str>); 4] = [
            ("不帶提示", None),
            ("只放講到的（17 詞）", Some(&ideal)),
            ("使用者詞表（9 詞）", Some(&vocab_only)),
            ("詞表＋業務名詞（26 詞）", Some(&vocab_plus)),
        ];

        let engine = Whisper::load(&model, 8).expect("載入模型");
        let mut runs: Vec<(String, usize, Vec<usize>, Vec<usize>)> = Vec::new();
        // 不足一秒的目錄會讓 div_ceil 回 0，下面的 buckets - 1 就負溢位
        let buckets = audio_s.div_ceil(BUCKET_S * 3).max(1);
        for (name, prompt) in configs {
            let mut text = String::new();
            let mut dens = vec![0usize; buckets];
            for (i, batch) in samples.chunks(OURS_BATCH_S * 16_000).enumerate() {
                let segs = engine.run(batch, prompt).expect("轉錄");
                let out: String = segs.iter().map(|s| s.text.as_str()).collect();
                let out = crate::stt::diff::to_traditional(&out, &Default::default());
                let b = ((i * OURS_BATCH_S) / (BUCKET_S * 3)).min(buckets - 1);
                dens[b] += out.chars().count();
                text.push_str(&out);
            }
            let hits: Vec<usize> = OURS.iter().map(|w| text.matches(w).count()).collect();
            eprintln!(
                "{name:<24} {:>5} 字，目標詞 {} 次",
                text.chars().count(),
                hits.iter().sum::<usize>()
            );
            runs.push((name.to_owned(), text.chars().count(), hits, dens));
        }

        eprint!("{:<12}", "詞");
        for (n, ..) in &runs {
            eprint!("{n:>16}");
        }
        eprintln!();
        for (i, w) in OURS.iter().enumerate() {
            eprint!("{w:<12}");
            for (.., h, _) in &runs {
                eprint!("{:>16}", h[i]);
            }
            eprintln!();
        }
        eprint!("{:<12}", "合計");
        for (.., h, _) in &runs {
            eprint!("{:>16}", h.iter().sum::<usize>());
        }
        eprintln!();

        let base = runs[0].3.clone();
        for (name, .., dens) in runs.iter().skip(1) {
            let collapsed: Vec<usize> = base
                .iter()
                .zip(dens)
                .enumerate()
                .filter(|(_, (b, g))| **b > 50 && **g * 2 < **b)
                .map(|(i, _)| i)
                .collect();
            eprintln!("{name} 相對於不帶提示塌掉的時間桶：{collapsed:?}");
        }
    }

    /// 掃過幾種詞表長度，看兩件事：有沒有整段被跳過，以及專有名詞轉得對不對。
    ///
    /// 不帶提示那組跑兩趟，其餘各一趟。兩趟是為了回答「下面的差異會不會只是
    /// 晃動」，而量到的答案是不會：重新載入模型再跑，結果逐字相同，連斷句數
    /// 都一樣。所以各組之間的差異可以直接讀，不需要先扣掉雜訊。
    ///
    /// （跨行程不保證同樣穩。曾經量到同一組設定一次 28 段、一次 20 段，字數
    /// 只差一個字、命中的詞完全相同 —— 猜是機器負載改變了 ggml 的執行緒切分。
    /// 斷句數因此不是可靠的判準，字數與命中才是。）
    #[test]
    #[ignore = "跑真實模型五趟並各自載入一次，一次約三分鐘。用 --ignored 執行"]
    fn probe_whether_an_initial_prompt_costs_content() {
        let Some((model, samples)) = material() else {
            eprintln!("略過：找不到模型或音訊");
            return;
        };
        let buckets = samples.len() / (16_000 * BUCKET_S) + 1;
        let terms: Vec<&str> = TERMS.split('、').collect();
        // 詞表長度掃描：0 是不帶提示，其餘是「七個關鍵詞再加幾個沒講到的」。
        // 第一組跑兩趟，用來證明後面的差異不是晃動。
        let sizes: [(Option<usize>, usize); 5] = [
            (None, 2),
            (Some(0), 1),
            (Some(8), 1),
            (Some(15), 1),
            (Some(30), 1),
        ];

        let mut runs: Vec<(String, Vec<usize>, Vec<bool>)> = Vec::new();
        for (extra, trips) in sizes {
            let prompt = extra.map(prompt_of);
            let label = match extra {
                None => "不帶提示".to_owned(),
                Some(n) => format!("{} 詞", terms.len() + n),
            };
            for trip in 1..=trips {
                let w = Whisper::load(&model, 8).expect("載入定稿模型");
                let segs = w.run(&samples, prompt.as_deref()).expect("轉錄");
                let text: String = segs.iter().map(|s| s.text.as_str()).collect();
                let hit: Vec<bool> = terms.iter().map(|t| text.contains(t)).collect();
                let d = density(&segs, buckets);
                println!(
                    "{label} 第{trip}趟：{} 段、{} 字、命中 {}/{}",
                    segs.len(),
                    text.chars().count(),
                    hit.iter().filter(|h| **h).count(),
                    terms.len()
                );
                println!("  每 {BUCKET_S} 秒的字數 {d:?}");
                println!("  {text}");
                runs.push((label.clone(), d, hit));
            }
        }

        // 「整段跳過內容」在這裡長什麼樣：某個時間桶在不帶提示的兩趟都有字，
        // 帶提示之後掉到幾乎沒有。整體字數看不出來，別處的碎裂會補回去。
        let floor: Vec<usize> = (0..buckets)
            .map(|i| runs[0].1[i].min(runs[1].1[i]))
            .collect();
        for (label, d, _) in &runs[2..] {
            let holes: Vec<usize> = (0..buckets)
                .filter(|&i| floor[i] >= 20 && d[i] * 4 < floor[i])
                .collect();
            println!("{label} 相對於不帶提示塌掉的時間桶（第幾個 {BUCKET_S} 秒）：{holes:?}");
        }

        // 每個詞在每一趟各自轉對了沒。前兩欄是同一組設定的兩趟，不一致就代表
        // 這一欄之後的差異都要當成雜訊讀。
        println!(
            "詞　　　{}",
            runs.iter()
                .map(|(l, ..)| l.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for (j, t) in terms.iter().enumerate() {
            let marks: Vec<&str> = runs
                .iter()
                .map(|(_, _, h)| if h[j] { "○" } else { "✗" })
                .collect();
            println!("{t}\t{}", marks.join("   "));
        }
    }
}
