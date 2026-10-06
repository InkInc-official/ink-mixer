use std::io::IsTerminal;

use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() {
    // RUST_LOG が未設定（または不正）なら info を既定にする
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // ログは stderr に出し、stdout はコマンドの出力（devices 一覧など）専用にする
    // 色付けは stderr が端末のときだけ（ファイルや pipe にエスケープコードを混ぜない）
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    info!(
        "Ink Mixer v{} (pre-alpha) on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
}
