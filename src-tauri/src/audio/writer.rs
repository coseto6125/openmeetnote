//! 把擷取到的 PCM 寫成分段 wav（BLUEPRINT.md §11 的 `audio_segments`）。
//!
//! 存音訊的理由不是「留個備份」，而是逐字稿沒有它就無法被驗證：轉錯一個字、
//! 漏掉一段發言，沒有原音就只能憑印象爭論，也沒辦法換模型或換參數重跑同一
//! 段話比較好壞。
//!
//! 幾個決定寫在這裡而不只是註解：
//!
//! - 每軌各自成檔。`track` 是「本機 vs 遠端」的先驗，混檔就再也分不開。
//! - 16-bit PCM 而不是 f32。檔案小一半，而 whisper 本來就要轉成 f32 再吃，
//!   多這一次轉換不損失可聽內容（f32 的來源本身就是 16-bit 裝置）。
//! - 定長分段。單檔太大時任何一次寫入失敗都會賠掉整場；分段之後壞的只是
//!   那一段，而且 §11 的索引本來就以段為單位。
//! - 寫檔在自己的執行緒。§12 要求音訊執行緒不做網路、不碰 SQLite、不呼叫
//!   UI，磁碟 I/O 雖然沒被列名，但它會阻塞，不該壓在分流的熱路徑上。

use std::path::{Path, PathBuf};

use crate::audio::{Chunk, SAMPLE_RATE};
use crate::model::Track;

/// 一段音訊的長度。
///
/// 一分鐘在兩個成本之間：檔案數量（一小時 60 個檔，索引查得動）與單段
/// 損失（寫壞一段丟掉一分鐘，不是一小時）。
const SEGMENT_MS: u64 = 60_000;

/// 寫完一段音訊之後要告訴 session 的事。
///
/// 不直接寫 SQLite：這裡是音訊執行緒（§12）。走 channel 回 session，
/// 由它配發 seq 並落地，跟逐字稿走同一條路。
#[derive(Debug, Clone)]
pub struct WrittenSegment {
    pub track: Track,
    pub path: String,
    pub captured_start_ms: u64,
    pub captured_end_ms: u64,
    pub checksum: String,
}

/// 累積某一軌的樣本，滿一段就落地。
struct TrackWriter {
    track: Track,
    dir: PathBuf,
    samples: Vec<f32>,
    start_ms: u64,
    /// 已經收到的樣本對應的結束位置，用來偵測裝置重開造成的時間跳躍。
    end_ms: u64,
    started: bool,
}

impl TrackWriter {
    fn new(track: Track, dir: PathBuf) -> Self {
        Self {
            track,
            dir,
            // 一段的長度是已知的，先配好：從空的長到 60 秒要經過 11 次
            // 重新配置，而且成長是倍增的，尖峰會多佔近一倍的容量。
            samples: Vec::with_capacity(SAMPLE_RATE as usize * SEGMENT_MS as usize / 1_000),
            start_ms: 0,
            end_ms: 0,
            started: false,
        }
    }

    fn push(&mut self, chunk: &Chunk) -> Option<WrittenSegment> {
        let chunk_ms = chunk.samples.len() as u64 * 1_000 / SAMPLE_RATE as u64;
        // 擷取時間往回跳或跳空，代表裝置重開過（§5.2 的 source_epoch）。
        // 把手上的東西先收掉，不要把兩個不連續的時段黏進同一個檔案。
        let discontinuous = self.started && chunk.captured_start_ms != self.end_ms;
        let flushed = if discontinuous { self.flush() } else { None };

        if !self.started {
            self.start_ms = chunk.captured_start_ms;
            self.end_ms = chunk.captured_start_ms;
            self.started = true;
        }
        self.samples.extend_from_slice(&chunk.samples);
        self.end_ms = chunk.captured_start_ms + chunk_ms;

        // 不連續時已經 flush 過，這一輪就不再檢查長度：一個 chunk 不會
        // 同時造成兩段落地。
        if flushed.is_some() {
            return flushed;
        }
        (self.end_ms - self.start_ms >= SEGMENT_MS)
            .then(|| self.flush())
            .flatten()
    }

    /// 把手上的樣本寫成一個檔案。沒有樣本時什麼都不做。
    fn flush(&mut self) -> Option<WrittenSegment> {
        if self.samples.is_empty() {
            self.started = false;
            return None;
        }
        let samples = std::mem::take(&mut self.samples);
        let (start, end) = (self.start_ms, self.end_ms);
        self.started = false;

        // 檔名帶軌道與起點：目錄列出來就看得懂順序，不必開資料庫。
        let path = self
            .dir
            .join(format!("{}-{start:09}.wav", self.track.as_str()));
        match write_wav(&path, &samples) {
            Ok(checksum) => Some(WrittenSegment {
                track: self.track,
                path: path.to_string_lossy().into_owned(),
                captured_start_ms: start,
                captured_end_ms: end,
                checksum,
            }),
            Err(e) => {
                // 寫不進去要說出來，但不能中斷錄音：逐字稿仍在跑，
                // 沒有音訊比整場停掉好。
                crate::stt::live::log(&format!("音訊寫入失敗（{}）：{e}", path.display()));
                None
            }
        }
    }
}

/// 寫出 16-bit PCM 單聲道 wav，回傳樣本內容的 SHA-256。
///
/// 用 `hound` 而不是自己組 header：讀取端（`stt::load_wav_16k_mono`）本來就
/// 是 hound，兩邊共用同一份格式知識，就不會有一邊改了另一邊讀不到的問題。
///
/// checksum 算的是樣本本身而不是整個檔案：header 之後換了寫法，同一段音訊的
/// 檢查碼不該因此改變。
fn write_wav(path: &Path, samples: &[f32]) -> std::io::Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 先夾再轉：超過 ±1.0 的樣本直接 as i16 會環繞，一個爆音會變成
    // 反相的雜訊。
    let pcm: Vec<i16> = samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();
    let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
    let checksum = crate::document::sha256_hex(&bytes);

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(std::io::Error::other)?;
    for v in &pcm {
        w.write_sample(*v).map_err(std::io::Error::other)?;
    }
    // finalize 會回頭補 header 的長度欄位。少了它，檔案的長度欄位停在 0，
    // 播放器與 hound 都會當成空檔。
    w.finalize().map_err(std::io::Error::other)?;

    Ok(checksum)
}

/// 收 chunk、寫檔、把落地的段回報出去，直到來源關閉。
///
/// 收尾時把兩軌手上剩的樣本都寫掉：那是使用者按下結束前最後幾十秒，
/// 丟掉它等於「最後一段永遠沒有原音」。
pub fn run(
    rx: std::sync::mpsc::Receiver<Chunk>,
    dir: PathBuf,
    tx: std::sync::mpsc::Sender<WrittenSegment>,
) {
    let mut writers = std::collections::HashMap::new();
    for chunk in rx {
        let w = writers
            .entry(chunk.track)
            .or_insert_with(|| TrackWriter::new(chunk.track, dir.clone()));
        if let Some(seg) = w.push(&chunk) {
            if tx.send(seg).is_err() {
                break;
            }
        }
    }
    for (_, mut w) in writers {
        if let Some(seg) = w.flush() {
            let _ = tx.send(seg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(track: Track, start_ms: u64, ms: u64) -> Chunk {
        let n = (SAMPLE_RATE as u64 * ms / 1_000) as usize;
        Chunk {
            track,
            captured_start_ms: start_ms,
            samples: vec![0.5; n].into(),
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("omn-writer-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn test_a_segment_lands_once_it_reaches_the_segment_length() {
        let dir = tempdir("len");
        let mut w = TrackWriter::new(Track::Mic, dir.clone());
        // 半段還不該落地，否則一小時會生出上千個碎檔
        assert!(w.push(&chunk(Track::Mic, 0, 30_000)).is_none());
        let seg = w
            .push(&chunk(Track::Mic, 30_000, 30_000))
            .expect("滿一段了卻沒有落地");
        assert_eq!((seg.captured_start_ms, seg.captured_end_ms), (0, 60_000));
        assert!(
            std::path::Path::new(&seg.path).exists(),
            "檔案沒有真的寫出來"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_a_device_restart_does_not_glue_two_time_ranges_into_one_file() {
        // 擷取時間跳空代表裝置重開過。黏在一起的話這個檔案的 captured 區間
        // 會宣稱涵蓋一段其實不存在的音訊，引用就會定位到錯的地方。
        let dir = tempdir("epoch");
        let mut w = TrackWriter::new(Track::System, dir.clone());
        assert!(w.push(&chunk(Track::System, 0, 5_000)).is_none());
        let seg = w
            .push(&chunk(Track::System, 90_000, 5_000))
            .expect("時間跳空之後該把前一段收掉");
        assert_eq!((seg.captured_start_ms, seg.captured_end_ms), (0, 5_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_the_tail_is_written_when_capture_stops() {
        // 使用者按下結束前的最後幾十秒不到一整段，丟掉就永遠沒有原音
        let dir = tempdir("tail");
        let mut w = TrackWriter::new(Track::Mic, dir.clone());
        assert!(w.push(&chunk(Track::Mic, 0, 3_000)).is_none());
        let seg = w.flush().expect("收尾沒有寫出剩下的音訊");
        assert_eq!(seg.captured_end_ms, 3_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_the_wav_header_describes_16bit_mono_at_the_capture_rate() {
        // 標頭寫錯的話播放器與 whisper 會用錯的取樣率解讀，聽起來像變速
        let dir = tempdir("header");
        let path = dir.join("h.wav");
        write_wav(&path, &[0.0; 1_600]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 1, "聲道數");
        assert_eq!(
            u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), 16, "位元深度");
        // 1600 個樣本 × 2 bytes
        assert_eq!(
            u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]),
            3_200
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_samples_beyond_full_scale_clamp_instead_of_wrapping() {
        // 直接 as i16 會讓 +1.5 環繞成負值，一個爆音變成反相雜訊
        let dir = tempdir("clamp");
        let path = dir.join("c.wav");
        write_wav(&path, &[1.5, -1.5]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let a = i16::from_le_bytes([bytes[44], bytes[45]]);
        let b = i16::from_le_bytes([bytes[46], bytes[47]]);
        assert_eq!(a, i16::MAX);
        assert_eq!(b, -i16::MAX);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 寫出去的檔案，這個 app 自己讀得回來。
    ///
    /// header 是手寫的，而讀取端是 `hound`（`stt::load_wav_16k_mono`）。兩邊
    /// 對取樣率、聲道數、位元深度的認知只要差一項，錄下來的原音就再也回不到
    /// 轉錄流程 —— 而「事後可以重跑」正是保存原音的唯一理由。
    #[test]
    fn test_what_the_writer_writes_the_reader_reads_back() {
        let dir = tempdir("roundtrip");
        let path = dir.join("rt.wav");
        // 涵蓋兩端與零：量化誤差最大的地方在滿刻度
        let src = [0.0_f32, 0.5, -0.5, 1.0, -1.0, 0.25];
        write_wav(&path, &src).unwrap();

        let back = crate::stt::load_wav_16k_mono(path.to_str().unwrap())
            .expect("自己寫的 wav 自己讀不回來");
        assert_eq!(back.len(), src.len(), "樣本數對不上");
        for (i, (a, b)) in src.iter().zip(back.iter()).enumerate() {
            // 寫入乘 i16::MAX、讀取除 32768，往返誤差是一個量化階
            assert!(
                (a - b).abs() < 1.0 / 32_767.0 + f32::EPSILON,
                "第 {i} 個樣本往返後差太多：寫入 {a}，讀回 {b}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_the_same_audio_always_gets_the_same_checksum() {
        // checksum 是「這個檔案還是當初那段音訊嗎」的唯一依據
        let dir = tempdir("sum");
        let a = write_wav(&dir.join("a.wav"), &[0.1, 0.2, 0.3]).unwrap();
        let b = write_wav(&dir.join("b.wav"), &[0.1, 0.2, 0.3]).unwrap();
        let c = write_wav(&dir.join("c.wav"), &[0.1, 0.2, 0.4]).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64, "SHA-256 的十六進位長度");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
