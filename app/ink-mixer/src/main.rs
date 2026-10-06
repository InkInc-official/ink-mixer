use std::io::IsTerminal;
use std::path::PathBuf;

use directories::ProjectDirs;
use ink_core::Config;
use tracing::{info, warn};
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

    let _config = load_config();
}

/// 設定ファイルの場所（OS 標準の設定ディレクトリ配下の `config.toml`）
fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "Ink Inc", "Ink Mixer").map(|dirs| dirs.config_dir().join("config.toml"))
}

/// 設定を読み込む。無ければデフォルトで作成する。
/// 読めないときは warn を出し、ファイルは書き換えずにデフォルト値で続行する。
fn load_config() -> Config {
    let Some(path) = config_path() else {
        warn!("config directory not found; using default settings");
        return Config::default();
    };
    match Config::load_or_create(&path) {
        Ok(config) => {
            info!(
                "config loaded from {} (version {})",
                path.display(),
                config.version
            );
            config
        }
        Err(e) => {
            warn!("{e} ({}); using default settings", path.display());
            Config::default()
        }
    }
}
