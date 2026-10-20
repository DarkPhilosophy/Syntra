//! Command line of the daemon binary.
//!
//! Argument parsing lives with the binary that owns the command line, not in
//! `syntra-core`: the core receives a typed [`ConfigOverrides`] and never
//! depends on an argument parser or on the CLI frontend crate.

use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand};
use syntra_cli::CliArgs;
use syntra_core::config::{CaptureBackend, ConfigOverrides, EmulationBackend, LONG_VERSION};

/// Parsed daemon invocation.
#[derive(Parser, Debug)]
#[command(author, version = LONG_VERSION, about, long_about = None)]
pub struct Args {
    /// the listen port for syntra
    #[arg(short, long)]
    port: Option<u16>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// capture backend override
    #[arg(long)]
    capture_backend: Option<CaptureBackend>,

    /// emulation backend override
    #[arg(long)]
    emulation_backend: Option<EmulationBackend>,

    /// path to non-default certificate location
    #[arg(long)]
    cert_path: Option<PathBuf>,

    /// subcommands
    #[command(subcommand)]
    command: Option<Command>,
}

/// What the binary was asked to do.
#[derive(Subcommand, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// test input emulation
    TestEmulation(TestEmulationArgs),
    /// test input capture
    TestCapture(TestCaptureArgs),
    /// Syntra commandline interface
    Cli(CliArgs),
    /// run in daemon mode
    Daemon,
}

/// Flags accepted by `test-emulation`.
///
/// Kept so existing invocations keep parsing; the test exercises every kind
/// of event regardless of which flags were given.
#[derive(ClapArgs, Clone, Debug, Eq, PartialEq)]
pub struct TestEmulationArgs {
    #[arg(long)]
    mouse: bool,
    #[arg(long)]
    keyboard: bool,
    #[arg(long)]
    scroll: bool,
}

/// Flags accepted by `test-capture` (none).
#[derive(ClapArgs, Clone, Debug, Eq, PartialEq)]
pub struct TestCaptureArgs {}

impl Args {
    /// The configuration overrides requested on the command line.
    pub fn overrides(&self) -> ConfigOverrides {
        ConfigOverrides {
            port: self.port,
            config: self.config.clone(),
            capture_backend: self.capture_backend,
            emulation_backend: self.emulation_backend,
            cert_path: self.cert_path.clone(),
        }
    }

    /// The requested command; `None` runs the service.
    pub fn command(&self) -> Option<Command> {
        self.command.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_become_overrides_without_a_subcommand() {
        let args = Args::try_parse_from([
            "syntra-daemon",
            "--port",
            "4242",
            "--config",
            "/tmp/c.toml",
            "--cert-path",
            "/tmp/c.pem",
        ])
        .expect("valid invocation");
        assert_eq!(args.command(), None);
        assert_eq!(
            args.overrides(),
            ConfigOverrides {
                port: Some(4242),
                config: Some(PathBuf::from("/tmp/c.toml")),
                capture_backend: None,
                emulation_backend: None,
                cert_path: Some(PathBuf::from("/tmp/c.pem")),
            }
        );
    }

    #[test]
    fn backend_override_uses_the_configuration_names() {
        let args = Args::try_parse_from(["syntra-daemon", "--capture-backend", "dummy"])
            .expect("dummy backend always exists");
        assert_eq!(
            args.overrides().capture_backend,
            Some(CaptureBackend::Dummy)
        );
    }

    #[test]
    fn legacy_test_subcommands_keep_parsing() {
        let args = Args::try_parse_from(["syntra-daemon", "test-emulation", "--mouse"])
            .expect("flags are still accepted");
        assert!(matches!(args.command(), Some(Command::TestEmulation(_))));
        let args = Args::try_parse_from(["syntra-daemon", "test-capture"]).expect("no flags");
        assert!(matches!(args.command(), Some(Command::TestCapture(_))));
        let args = Args::try_parse_from(["syntra-daemon", "daemon"]).expect("explicit daemon");
        assert_eq!(args.command(), Some(Command::Daemon));
    }
}
