fn main() {
    link_swift_runtime_on_macos();
    tauri_build::build()
}

/// macOS 上要自己補 Swift runtime 的 rpath。
///
/// `screencapturekit` 的 Swift bridge 依賴 `libswift_Concurrency.dylib`，它的
/// build.rs 也確實發了 `cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift` ——
/// 但 build script 的 link arg 只作用在**發出它的那個 package 自己的** target
/// 上，不會傳到下游的執行檔。於是連結成功、打包成功，程式在 dyld 階段就
/// SIGABRT，連 main 都還沒進去。
///
/// 只加 `/usr/lib/swift`（作業系統自帶，macOS 10.14.4 起就有，遠低於本專案
/// 的 13.0 下限），不加 Xcode toolchain 底下那條：那是建置機器的路徑，烤進
/// 要發給別人的二進位裡沒有意義。
fn link_swift_runtime_on_macos() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
