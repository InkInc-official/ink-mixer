mod bgm_worker;
mod cli;
mod devices;
mod gui;
mod library_cmd;
mod library_view;
mod run;
mod se_loader;
mod session;
mod settings;
mod sound_state;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use directories::ProjectDirs;
use ink_backend::CpalBackend;
use ink_core::Config;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    // --help / --version はここで表示して終了する（ログより先）
    let cli = Cli::parse();

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

    let Loaded { config, save_to } = load_config();

    match cli.command {
        Some(Command::Devices) => devices::run(&CpalBackend::new(), &config),
        Some(Command::Run(args)) => run::run(&CpalBackend::new(), &config, &args),
        Some(Command::Library(args)) => library_cmd::run(&args),
        None => gui::run(&config, save_to),
    }
}

/// The configuration for this run, and where the window may save it.
struct Loaded {
    /// The configuration, or the defaults when the file could not be read.
    config: Config,
    /// `Ok(path)` when the settings are saved there. `Err(reason)` when they
    /// must not be saved, because a file that could not be read would be
    /// overwritten with the defaults; `reason` is shown in the window.
    save_to: Result<PathBuf, String>,
}

/// 設定ファイルの場所（OS 標準の設定ディレクトリ配下の `config.toml`）
fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "Ink Inc", "Ink Mixer").map(|dirs| dirs.config_dir().join("config.toml"))
}

/// 設定を読み込む。無ければデフォルトで作成する。
/// 読めないときは warn を出し、ファイルは書き換えずにデフォルト値で続行する。
/// その実行では設定を保存しない（読めないファイルをデフォルトで上書きしない）。
fn load_config() -> Loaded {
    load_from(config_path())
}

fn load_from(path: Option<PathBuf>) -> Loaded {
    let Some(path) = path else {
        warn!("config directory not found; using default settings");
        return Loaded {
            config: Config::default(),
            save_to: Err("設定ファイルの場所が見つからないため、設定は保存されません".to_string()),
        };
    };
    match Config::load_or_create(&path) {
        Ok(config) => {
            info!(
                "config loaded from {} (version {})",
                path.display(),
                config.version
            );
            Loaded {
                config,
                save_to: Ok(path),
            }
        }
        Err(e) => {
            warn!("{e} ({}); using default settings", path.display());
            Loaded {
                config: Config::default(),
                save_to: Err(format!(
                    "設定ファイル（{}）を読めないため、設定は保存されません。直すか、削除してください",
                    path.display()
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ink-mixer-main-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_is_created_and_settings_are_saved_there() {
        let path = test_dir("create").join("sub").join("config.toml");
        let loaded = load_from(Some(path.clone()));
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.save_to, Ok(path.clone()));
        assert!(path.exists());
    }

    #[test]
    fn a_version_1_file_with_levels_is_loaded() {
        let path = test_dir("v1").join("config.toml");
        fs::write(
            &path,
            "version = 1\n[audio]\ninput_device = \"alsa:default\"\n[mixer]\nmaster_db = -20.0\n",
        )
        .unwrap();
        let loaded = load_from(Some(path.clone()));
        assert_eq!(loaded.config.mixer.master_db(), Some(-20.0));
        assert_eq!(loaded.save_to, Ok(path));
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_overwritten_and_not_saved() {
        let path = test_dir("broken").join("config.toml");
        fs::write(&path, "broken").unwrap();
        let loaded = load_from(Some(path.clone()));

        assert_eq!(loaded.config, Config::default());
        assert_eq!(
            loaded.save_to,
            Err(format!(
                "設定ファイル（{}）を読めないため、設定は保存されません。直すか、削除してください",
                path.display()
            ))
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken");
    }

    #[test]
    fn a_newer_version_is_not_overwritten_and_not_saved() {
        let path = test_dir("newer").join("config.toml");
        let text = "version = 99\nsomething_new = true\n";
        fs::write(&path, text).unwrap();
        let loaded = load_from(Some(path.clone()));
        assert!(loaded.save_to.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn no_config_directory_means_no_saving() {
        let loaded = load_from(None);
        assert_eq!(loaded.config, Config::default());
        assert_eq!(
            loaded.save_to,
            Err("設定ファイルの場所が見つからないため、設定は保存されません".to_string())
        );
    }
}
