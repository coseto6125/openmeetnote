//! whisper 在無語音音訊上的幻覺（實測迴歸）。
//!
//! 兩小時 soak 漏過兩筆「字幕志愿者 XXX」，都在沒人說話的麥克風軌上，
//! RMS 剛好高過能量門檻。這裡用真實模型重現它，確認判準擋得住。

use openmeetnote_lib::stt::{is_hallucination, whisper::Whisper};

fn model() -> Option<Whisper> {
    let p = std::path::PathBuf::from(
        std::env::var("OMN_TEST_ASSETS").unwrap_or_else(|_| "/home/enor/whisper-bench".into()),
    )
    .join("models/ggml-large-v3-turbo-q5_0.bin");
    p.exists()
        .then(|| Whisper::load(p.to_str().unwrap(), 4).expect("載入模型"))
}

#[test]
fn test_whisper_invents_subtitle_credits_on_silence_and_the_filter_catches_them() {
    let Some(w) = model() else {
        eprintln!("略過：找不到模型");
        return;
    };

    // 純靜音是最乾淨的重現：沒有任何訊號，模型仍會產生訓練資料殘留
    let segs = w.transcribe(&vec![0.0f32; 16_000 * 8]).expect("轉錄靜音");
    println!(
        "靜音轉出 {} 句：{:?}",
        segs.len(),
        segs.iter().map(|s| &s.text).collect::<Vec<_>>()
    );

    // 模型行為會隨版本變動，所以斷言的是「若它編了，我們擋得住」，
    // 而不是「它一定會編」—— 後者是在測 whisper 不是測我們的程式碼。
    let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
    if texts
        .iter()
        .any(|t| t.contains("字幕") || t.contains("观看") || t.contains("觀看"))
    {
        assert!(
            is_hallucination(&texts, 0.0),
            "靜音產生的 {texts:?} 沒有被判為幻覺"
        );
    }
}

#[test]
fn test_real_speech_survives_the_filter() {
    let Some(w) = model() else { return };
    let dir = std::path::PathBuf::from(
        std::env::var("OMN_TEST_ASSETS").unwrap_or_else(|_| "/home/enor/whisper-bench".into()),
    );
    let wav = dir.join("near.wav");
    if !wav.exists() {
        eprintln!("略過：找不到音訊");
        return;
    }
    let s = openmeetnote_lib::stt::load_wav_16k_mono(wav.to_str().unwrap()).expect("讀音訊");
    let rms = (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
    let segs = w.transcribe(&s[..16_000 * 30]).expect("轉錄語音");

    let texts: Vec<&str> = segs.iter().map(|x| x.text.as_str()).collect();
    assert!(
        !is_hallucination(&texts, rms),
        "真實語音被誤判成幻覺：{texts:?}"
    );
    println!("真實語音 {} 句、RMS {rms:.4}，零誤殺", segs.len());
}
