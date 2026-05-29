#![forbid(unsafe_code)]

use sakurasato_core::Config;

fn main() {
    // M5 で ratatui ベースの本体に差し替える。M1 では型解決の検証のみ。
    let _ = std::any::type_name::<Config>();
    println!("sakurasato-tui (M1 skeleton) — see milestone M5 for the real client");
}
