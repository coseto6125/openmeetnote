mod agent;
mod clock;
mod config;
mod db;
mod document;
mod model;
mod session;
mod store;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(session::SessionHandle::default())
        .manage(config::ConfigHandle::default())
        .setup(|app| {
            // 資料庫放 app data dir，不是安裝目錄：安裝目錄在 Windows 需要
            // 提權才能寫，而且解除安裝會連同會議紀錄一起清掉。
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            app.manage(store::StoreHandle::open(dir.join("openmeetnote.sqlite3"))?);

            session::spawn_pump(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            session::start_meeting,
            session::pause_meeting,
            session::resume_meeting,
            session::add_note,
            session::confirm_speaker,
            session::edit_transcript,
            session::create_snapshot,
            session::stop_meeting,
            session::resync,
            session::inject_fault,
            session::new_meeting,
            session::active_meeting,
            session::list_meetings,
            session::open_meeting,
            session::rename_meeting,
            session::delete_meeting,
            session::snapshot_document,
            session::export_document,
            session::rebuild_projections,
            config::get_settings,
            config::save_provider,
            config::save_secret,
            config::clear_secret,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
