//! コマンドライン引数の定義。

use clap::{Args, Parser, Subcommand};

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
            }))
        );
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
