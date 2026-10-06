//! コマンドライン引数の定義。

use clap::{Parser, Subcommand};

/// Ink Mixer: extensible real-time audio console for streamers
#[derive(Debug, Parser)]
#[command(name = "ink-mixer", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// List audio input and output devices
    Devices,
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
    fn parses_devices_subcommand() {
        let cli = Cli::try_parse_from(["ink-mixer", "devices"]).unwrap();
        assert_eq!(cli.command, Some(Command::Devices));
    }

    #[test]
    fn no_subcommand_is_allowed() {
        let cli = Cli::try_parse_from(["ink-mixer"]).unwrap();
        assert_eq!(cli.command, None);
    }
}
