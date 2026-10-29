//! コマンドライン引数の定義。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum, ValueHint};
use ink_core::AssetKind;
use ink_core::graph::BGM_VOLUME_RANGE;

/// Ink Mixer: extensible real-time audio console for streamers
#[derive(Debug, Parser)]
#[command(name = "ink-mixer", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum Command {
    /// List audio input and output devices
    Devices,
    /// Play the microphone through the output (MIC -> Gain -> MASTER). Use headphones.
    /// The devices come from the config, but the levels are not read from the config
    Run(RunArgs),
    /// The sound library: the BGM and SE folders of Ink Mixer and their index
    Library(LibraryArgs),
}

/// `library` の引数。
#[derive(Debug, PartialEq, Args)]
pub struct LibraryArgs {
    /// Use this folder as the library instead of the default one (see `library path`).
    /// For checking and tests; it can be given before or after the action
    #[arg(
        long,
        value_name = "DIR",
        global = true,
        value_hint = ValueHint::DirPath
    )]
    pub library_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub action: LibraryAction,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum LibraryAction {
    /// Print the folder of the library (the BGM and SE folders are under `sounds`)
    Path,
    /// List the sounds, one per line: name, ID, kind, state, length, path
    List,
    /// Look at the sound folders for files that were put in by hand, and save what was found
    Scan,
    /// Copy a sound file (WAV or MP3) into the library
    Import(ImportArgs),
}

/// `library import` の引数。
#[derive(Debug, PartialEq, Args)]
pub struct ImportArgs {
    /// The sound file to copy (the original is not changed)
    #[arg(value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub file: PathBuf,
    /// Which folder it goes to: `bgm` (a song, any length) or `se` (a sound effect, up to 60 s)
    #[arg(long, value_enum, ignore_case = true, value_name = "KIND")]
    pub kind: KindArg,
}

/// The kind of a sound on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Bgm,
    Se,
}

impl From<KindArg> for AssetKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Bgm => Self::Bgm,
            KindArg::Se => Self::Se,
        }
    }
}

/// `run` の引数。
#[derive(Debug, PartialEq, Args)]
pub struct RunArgs {
    /// Input device ID (see `devices`). Defaults to the configured, then the default, input
    #[arg(long, value_name = "ID")]
    pub input: Option<String>,
    /// Output device ID (see `devices`). Defaults to the configured, then the default, output
    #[arg(long, value_name = "ID")]
    pub output: Option<String>,
    /// MIC gain in dB (-60 to +24). Levels are not read from the config
    #[arg(
        long,
        value_name = "DB",
        default_value_t = 0.0,
        allow_negative_numbers = true
    )]
    pub gain: f32,
    /// Master volume in dB (-60 to +6). Levels are not read from the config
    #[arg(long, value_name = "DB", default_value_t = -12.0, allow_negative_numbers = true)]
    pub master: f32,
    /// How long to run, in seconds
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub seconds: u64,
    /// Play this sound effect (WAV or MP3, up to 60 s) again and again through the SE channel
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub se: Option<PathBuf>,
    /// Seconds between the starts of the sound effect (more than 0). Needs --se
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 2.0,
        value_parser = positive_seconds,
        requires = "se"
    )]
    pub se_every: f64,
    /// Volume of the SE channel in dB (-60 to +6). Needs --se
    #[arg(
        long,
        value_name = "DB",
        default_value_t = 0.0,
        allow_negative_numbers = true,
        requires = "se"
    )]
    pub se_volume: f32,
    /// Play this song (WAV or MP3, any length) through the BGM channel, read a little at a time
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub bgm: Option<PathBuf>,
    /// Start the song again when it ends. Needs --bgm
    #[arg(long, requires = "bgm")]
    pub bgm_loop: bool,
    /// Volume of the BGM channel in dB (-60 to +6). Needs --bgm
    #[arg(
        long,
        value_name = "DB",
        default_value_t = 0.0,
        allow_negative_numbers = true,
        value_parser = bgm_volume_db,
        requires = "bgm"
    )]
    pub bgm_volume: f32,
    /// For checking on a real device: every SECONDS (more than 1), pause the song and play it
    /// again after 1 s; every second time, stop it and play it from the start. Needs --bgm
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = cycle_seconds,
        requires = "bgm"
    )]
    pub bgm_cycle: Option<f64>,
}

/// Parses a number of seconds that is finite and more than 0.
fn positive_seconds(text: &str) -> Result<f64, String> {
    let seconds: f64 = text
        .parse()
        .map_err(|_| format!("`{text}` is not a number"))?;
    if seconds.is_finite() && seconds > 0.0 {
        Ok(seconds)
    } else {
        Err(format!("`{text}` must be more than 0"))
    }
}

/// Parses a volume in dB that the BGM channel has: finite, from the lowest
/// to the highest value of [`BGM_VOLUME_RANGE`].
fn bgm_volume_db(text: &str) -> Result<f32, String> {
    let db: f32 = text
        .parse()
        .map_err(|_| format!("`{text}` is not a number"))?;
    let range = BGM_VOLUME_RANGE;
    if db.is_finite() && db >= range.min && db <= range.max {
        Ok(db)
    } else {
        Err(format!(
            "`{text}` must be between {:+} and {:+} dB",
            range.min, range.max
        ))
    }
}

/// Parses the seconds between two steps of `--bgm-cycle`: finite and more
/// than 1, because a pause lasts 1 s and must end before the next step.
fn cycle_seconds(text: &str) -> Result<f64, String> {
    let seconds: f64 = text
        .parse()
        .map_err(|_| format!("`{text}` is not a number"))?;
    if seconds.is_finite() && seconds > 1.0 {
        Ok(seconds)
    } else {
        Err(format!("`{text}` must be more than 1"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_help_says_the_levels_are_not_read_from_the_config() {
        let command = Cli::command();
        let run = command.find_subcommand("run").unwrap();
        let about = run.get_about().unwrap().to_string();
        assert!(
            about.contains("levels are not read from the config"),
            "{about}"
        );
        for name in ["gain", "master"] {
            let arg = run.get_arguments().find(|a| a.get_id() == name).unwrap();
            let help = arg.get_help().unwrap().to_string();
            assert!(help.contains("not read from the config"), "{name}: {help}");
        }
    }

    #[test]
    fn parses_devices_subcommand() {
        let cli = Cli::try_parse_from(["ink-mixer", "devices"]).unwrap();
        assert_eq!(cli.command, Some(Command::Devices));
    }

    #[test]
    fn run_has_defaults() {
        let cli = Cli::try_parse_from(["ink-mixer", "run"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Run(RunArgs {
                input: None,
                output: None,
                gain: 0.0,
                master: -12.0,
                seconds: 30,
                se: None,
                se_every: 2.0,
                se_volume: 0.0,
                bgm: None,
                bgm_loop: false,
                bgm_volume: 0.0,
                bgm_cycle: None,
            }))
        );
    }

    fn run_args(args: &[&str]) -> Result<RunArgs, clap::Error> {
        let mut full = vec!["ink-mixer", "run"];
        full.extend_from_slice(args);
        match Cli::try_parse_from(full)?.command {
            Some(Command::Run(args)) => Ok(args),
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn run_accepts_a_sound_effect_with_its_options() {
        let args = run_args(&[
            "--se",
            "sounds/clap.wav",
            "--se-every",
            "0.5",
            "--se-volume",
            "-6",
        ])
        .unwrap();
        assert_eq!(args.se, Some(PathBuf::from("sounds/clap.wav")));
        assert_eq!(args.se_every, 0.5);
        assert_eq!(args.se_volume, -6.0);
    }

    #[test]
    fn a_sound_effect_alone_uses_the_default_options() {
        let args = run_args(&["--se", "clap.mp3"]).unwrap();
        assert_eq!(args.se, Some(PathBuf::from("clap.mp3")));
        assert_eq!(args.se_every, 2.0);
        assert_eq!(args.se_volume, 0.0);
    }

    #[test]
    fn the_se_options_need_se() {
        for option in [["--se-every", "1"], ["--se-volume", "-3"]] {
            let error = run_args(&option).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{option:?}"
            );
        }
    }

    #[test]
    fn se_every_must_be_a_number_more_than_0() {
        for bad in ["0", "-1", "-0.5", "abc", "nan", "inf", ""] {
            let result = run_args(&["--se", "a.wav", "--se-every", bad]);
            assert!(result.is_err(), "{bad:?} was accepted");
        }
        for good in ["0.01", "1", "2.5", "3600"] {
            let args = run_args(&["--se", "a.wav", "--se-every", good]).unwrap();
            assert_eq!(args.se_every, good.parse::<f64>().unwrap());
        }
    }

    #[test]
    fn run_help_describes_the_se_options() {
        let command = Cli::command();
        let run = command.find_subcommand("run").unwrap();
        for name in ["se", "se_every", "se_volume"] {
            let arg = run.get_arguments().find(|a| a.get_id() == name).unwrap();
            assert!(arg.get_help().is_some(), "{name} has no help");
        }
        let every = run
            .get_arguments()
            .find(|a| a.get_id() == "se_every")
            .unwrap();
        assert!(every.get_help().unwrap().to_string().contains("Needs --se"));
    }

    #[test]
    fn run_accepts_a_song_with_its_options() {
        let args = run_args(&[
            "--bgm",
            "songs/a.mp3",
            "--bgm-loop",
            "--bgm-volume",
            "-18",
            "--bgm-cycle",
            "6",
        ])
        .unwrap();
        assert_eq!(args.bgm, Some(PathBuf::from("songs/a.mp3")));
        assert!(args.bgm_loop);
        assert_eq!(args.bgm_volume, -18.0);
        assert_eq!(args.bgm_cycle, Some(6.0));
    }

    #[test]
    fn a_song_alone_uses_the_default_options() {
        let args = run_args(&["--bgm", "a.wav"]).unwrap();
        assert_eq!(args.bgm, Some(PathBuf::from("a.wav")));
        assert!(!args.bgm_loop);
        assert_eq!(args.bgm_volume, 0.0);
        assert_eq!(args.bgm_cycle, None);
    }

    #[test]
    fn the_bgm_options_need_bgm() {
        for option in [
            vec!["--bgm-loop"],
            vec!["--bgm-volume", "-3"],
            vec!["--bgm-cycle", "4"],
        ] {
            let error = run_args(&option).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{option:?}"
            );
        }
    }

    #[test]
    fn a_song_can_be_played_with_a_sound_effect() {
        let args = run_args(&["--bgm", "a.wav", "--se", "b.wav"]).unwrap();
        assert_eq!(args.bgm, Some(PathBuf::from("a.wav")));
        assert_eq!(args.se, Some(PathBuf::from("b.wav")));
    }

    /// Formats a number for the command line the way a person writes it.
    fn text(value: f32) -> String {
        format!("{value}")
    }

    #[test]
    fn bgm_volume_must_be_in_the_range_of_the_bgm_channel() {
        let min = BGM_VOLUME_RANGE.min;
        let max = BGM_VOLUME_RANGE.max;
        let good = [
            text(min),
            "-18.5".to_string(),
            "0".to_string(),
            "-0".to_string(),
            "0.0".to_string(),
            text(max),
            format!("+{max}"),
        ];
        for value in &good {
            let args = run_args(&["--bgm", "a.wav", &format!("--bgm-volume={value}")])
                .unwrap_or_else(|e| panic!("{value:?} was refused: {e}"));
            assert_eq!(args.bgm_volume, value.parse::<f32>().unwrap(), "{value}");
        }
        let bad = [
            text(max + 0.5),
            text(min - 0.5),
            "-100".to_string(),
            "nan".to_string(),
            "NaN".to_string(),
            "NAN".to_string(),
            "inf".to_string(),
            "Inf".to_string(),
            "INF".to_string(),
            "-inf".to_string(),
            "-Inf".to_string(),
            "infinity".to_string(),
            "abc".to_string(),
            String::new(),
        ];
        for value in &bad {
            let error = run_args(&["--bgm", "a.wav", &format!("--bgm-volume={value}")])
                .expect_err(&format!("{value:?} was accepted"));
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_bgm_volume_error_names_the_range() {
        let error = run_args(&["--bgm", "a.wav", "--bgm-volume=7"]).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("must be between -60 and +6 dB"),
            "{message}"
        );
    }

    #[test]
    fn bgm_cycle_must_be_more_than_one_second() {
        for bad in ["1", "0.5", "0", "-1", "nan", "inf", "abc", ""] {
            let result = run_args(&["--bgm", "a.wav", &format!("--bgm-cycle={bad}")]);
            assert!(result.is_err(), "{bad:?} was accepted");
        }
        for good in ["1.01", "1.5", "2", "3600"] {
            let args = run_args(&["--bgm", "a.wav", "--bgm-cycle", good]).unwrap();
            assert_eq!(args.bgm_cycle, Some(good.parse::<f64>().unwrap()));
        }
    }

    #[test]
    fn run_help_describes_the_bgm_options() {
        let command = Cli::command();
        let run = command.find_subcommand("run").unwrap();
        let help = |name: &str| {
            run.get_arguments()
                .find(|a| a.get_id() == name)
                .unwrap_or_else(|| panic!("no argument {name}"))
                .get_help()
                .unwrap_or_else(|| panic!("{name} has no help"))
                .to_string()
        };
        assert!(help("bgm").contains("any length"));
        for name in ["bgm_loop", "bgm_volume", "bgm_cycle"] {
            assert!(help(name).contains("Needs --bgm"), "{name}");
        }
        assert!(
            help("bgm_cycle").contains("For checking on a real device"),
            "{}",
            help("bgm_cycle")
        );
        // The range in the help is the range of the channel.
        let range = format!("({} to {:+})", BGM_VOLUME_RANGE.min, BGM_VOLUME_RANGE.max);
        assert!(help("bgm_volume").contains(&range), "{range}");
    }

    #[test]
    fn run_accepts_devices_and_negative_levels() {
        let cli = Cli::try_parse_from([
            "ink-mixer",
            "run",
            "--input",
            "alsa:sysdefault:CARD=Headset",
            "--output",
            "alsa:sysdefault:CARD=Headset",
            "--gain",
            "-6",
            "--master",
            "-20.5",
            "--seconds",
            "180",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.input.as_deref(), Some("alsa:sysdefault:CARD=Headset"));
        assert_eq!(args.output.as_deref(), Some("alsa:sysdefault:CARD=Headset"));
        assert_eq!(args.gain, -6.0);
        assert_eq!(args.master, -20.5);
        assert_eq!(args.seconds, 180);
    }

    fn library_args(args: &[&str]) -> Result<LibraryArgs, clap::Error> {
        let mut full = vec!["ink-mixer", "library"];
        full.extend_from_slice(args);
        match Cli::try_parse_from(full)?.command {
            Some(Command::Library(args)) => Ok(args),
            other => panic!("expected library, got {other:?}"),
        }
    }

    #[test]
    fn library_has_four_actions() {
        for (name, action) in [
            ("path", LibraryAction::Path),
            ("list", LibraryAction::List),
            ("scan", LibraryAction::Scan),
        ] {
            let args = library_args(&[name]).unwrap();
            assert_eq!(args.action, action, "{name}");
            assert_eq!(args.library_dir, None, "{name}");
        }
        let args = library_args(&["import", "song.mp3", "--kind", "bgm"]).unwrap();
        assert_eq!(
            args.action,
            LibraryAction::Import(ImportArgs {
                file: PathBuf::from("song.mp3"),
                kind: KindArg::Bgm,
            })
        );
    }

    #[test]
    fn library_needs_an_action() {
        let error = library_args(&[]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn import_needs_a_file_and_a_kind() {
        let error = library_args(&["import", "--kind", "se"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        let error = library_args(&["import", "clap.wav"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(error.to_string().contains("--kind"), "{error}");
    }

    #[test]
    fn the_kind_is_bgm_or_se_in_any_case() {
        for (text, kind) in [
            ("bgm", KindArg::Bgm),
            ("se", KindArg::Se),
            ("BGM", KindArg::Bgm),
            ("Se", KindArg::Se),
        ] {
            let args = library_args(&["import", "a.wav", "--kind", text]).unwrap();
            let LibraryAction::Import(import) = args.action else {
                panic!("expected import");
            };
            assert_eq!(import.kind, kind, "{text}");
        }
        for bad in ["music", "", "bgm2", "sfx"] {
            let error = library_args(&["import", "a.wav", "--kind", bad]).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::InvalidValue,
                "{bad:?}"
            );
            assert_eq!(error.exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn the_kind_on_the_command_line_is_the_kind_of_the_library() {
        assert_eq!(AssetKind::from(KindArg::Bgm), AssetKind::Bgm);
        assert_eq!(AssetKind::from(KindArg::Se), AssetKind::Se);
    }

    #[test]
    fn the_library_dir_can_be_given_before_or_after_the_action() {
        let before = library_args(&["--library-dir", "/tmp/lib", "list"]).unwrap();
        let after = library_args(&["list", "--library-dir", "/tmp/lib"]).unwrap();
        assert_eq!(before, after);
        assert_eq!(before.library_dir, Some(PathBuf::from("/tmp/lib")));

        let import = library_args(&[
            "import",
            "a.wav",
            "--kind",
            "se",
            "--library-dir=relative/lib",
        ])
        .unwrap();
        assert_eq!(import.library_dir, Some(PathBuf::from("relative/lib")));
    }

    #[test]
    fn the_help_says_what_library_dir_is_for() {
        let command = Cli::command();
        let library = command.find_subcommand("library").unwrap();
        let arg = library
            .get_arguments()
            .find(|a| a.get_id() == "library_dir")
            .unwrap();
        let help = arg.get_help().unwrap().to_string();
        assert!(help.contains("instead of the default"), "{help}");
        assert!(help.contains("For checking and tests"), "{help}");
        // Every action has a help line, and so does the kind.
        for name in ["path", "list", "scan", "import"] {
            let action = library.find_subcommand(name).unwrap();
            assert!(action.get_about().is_some(), "{name} has no help");
        }
        let import = library.find_subcommand("import").unwrap();
        let kind = import
            .get_arguments()
            .find(|a| a.get_id() == "kind")
            .unwrap();
        let help = kind.get_help().unwrap().to_string();
        assert!(help.contains("bgm") && help.contains("se"), "{help}");
    }

    #[test]
    fn no_subcommand_is_allowed() {
        let cli = Cli::try_parse_from(["ink-mixer"]).unwrap();
        assert_eq!(cli.command, None);
    }
}
