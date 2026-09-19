pub mod agent;
mod audio;
pub mod cli_planner;
mod clock;
pub mod config;
pub mod db;
pub mod document;
pub mod model;
mod session;
pub mod sink;
pub mod store;
pub mod stt;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(session::SessionHandle::default())
        .manage(config::ConfigHandle::default())
        .manage(sink::SinkHandle::default())
        .setup(|app| {
            // 資料庫放 app data dir，不是安裝目錄：安裝目錄在 Windows 需要
            // 提權才能寫，而且解除安裝會連同會議紀錄一起清掉。
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let store = store::StoreHandle::open(dir.join("openmeetnote.sqlite3"))?;
            // 上次沒有正常結束的會議在這裡收尾。放在 pump 啟動之前：
            // 事件泵一旦跑起來就會開始寫入，那時再改狀態會跟新會議打架。
            if let Ok(mut st) = store.exclusive() {
                match st.close_abandoned_meetings() {
                    Ok(n) if n > 0 => {
                        stt::live::log(&format!("啟動時收尾了 {n} 場沒有正常結束的會議"));
                    }
                    Ok(_) => {}
                    Err(e) => stt::live::log(&format!("收尾未完成會議失敗：{e}")),
                }
                // 生成也一樣：關在半路的那一筆會永遠停在「生成中」
                match st.close_abandoned_runs() {
                    Ok(n) if n > 0 => {
                        stt::live::log(&format!("啟動時收尾了 {n} 筆沒有跑完的生成"));
                    }
                    Ok(_) => {}
                    Err(e) => stt::live::log(&format!("收尾未完成生成失敗：{e}")),
                }
            }
            // sink 的目標在 manage 之前讀出來：之後 store 的所有權已經交出去。
            // 讀不到就當成沒設定 —— 一條轉發管線不該擋住程式啟動。
            // 環境變數優先於資料庫：沒有設定畫面的時候，這是唯一不用改
            // 資料庫就能打開轉發的入口，也讓同一份安裝在不同啟動方式下
            // 可以開關。
            let sink_url = std::env::var("OPENMEETNOTE_SINK_URL")
                .ok()
                .filter(|u| !u.trim().is_empty())
                .or_else(|| {
                    store
                        .exclusive()
                        .ok()
                        .and_then(|st| st.app_setting(config::SINK_URL).ok().flatten())
                        .filter(|u| !u.trim().is_empty())
                });
            app.manage(store);

            if let Some(url) = sink_url {
                stt::live::log(&format!("事件 sink 轉發到 {url}"));
                app.state::<sink::SinkHandle>().set_url(
                    Some(url),
                    Some(session::sink_snapshot(app.handle().clone())),
                    Some(session::sink_command_handler(app.handle().clone())),
                );
            }

            session::spawn_pump(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            session::start_meeting,
            session::pause_meeting,
            session::resume_meeting,
            session::add_note,
            session::confirm_speaker,
            session::merge_speaker,
            session::edit_transcript,
            session::create_snapshot,
            session::stop_meeting,
            session::resync,
            session::inject_fault,
            session::new_meeting,
            session::active_meeting,
            session::list_meetings,
            session::search_meetings,
            session::reveal_export,
            session::summarize_meeting,
            session::open_meeting,
            session::rename_meeting,
            session::delete_meeting,
            session::delete_meeting_audio,
            session::get_keep_audio,
            session::set_keep_audio,
            session::snapshot_document,
            session::export_document,
            session::rebuild_projections,
            config::get_settings,
            config::list_llm_backends,
            config::save_provider,
            config::save_secret,
            config::clear_secret,
            config::get_sink_url,
            config::set_sink_url,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
