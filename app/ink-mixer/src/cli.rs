//! コマンドライン引数の定義。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueHint};

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

    #[test]
    fn no_subcommand_is_allowed() {
        let cli = Cli::try_parse_from(["ink-mixer"]).unwrap();
        assert_eq!(cli.command, None);
    }
}
