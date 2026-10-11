use std::path::PathBuf;
use std::process::exit;

use clap::{Args, Parser, Subcommand};

#[allow(dead_code)]
mod audio;
#[allow(dead_code)] // TODO(wiring todo): remove once `serve` calls config::load.
mod config;
#[allow(dead_code)]
mod engine;
#[allow(dead_code)]
mod protocol;
#[allow(dead_code)]
mod postprocess;
#[allow(dead_code)]
mod sys;

/// Local speech-to-text daemon and client.
#[derive(Parser, Debug)]
#[command(name = "ursula", version)]
struct Cli {
    /// Daemon socket path (default: $XDG_RUNTIME_DIR/ursula.sock).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Config file path (default: $XDG_CONFIG_HOME/ursula/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the daemon.
    Serve,
    /// Start a mic session and block until it finishes.
    Record(RecordArgs),
    /// Stop the active session.
    Stop,
    /// Cancel the active session.
    Cancel,
    /// Stop if a session is active, else record.
    Toggle(RecordArgs),
    /// Transcribe a WAV file and print the text.
    File {
        #[arg(value_name = "PATH.wav")]
        path: PathBuf,
    },
    /// Print the status JSON.
    Status,
    /// Pipe NDJSON from stdin to the socket and print events.
    Raw,
}

#[derive(Args, Debug)]
struct RecordArgs {
    /// Print one segment per line as they arrive.
    #[arg(long, conflicts_with = "post")]
    stream: bool,
    /// Post-processor to run on the final text (repeatable).
    #[arg(long, value_name = "NAME")]
    post: Vec<String>,
    /// Hotword to boost (repeatable).
    #[arg(long, value_name = "W")]
    hotword: Vec<String>,
    /// Context text passed to post-processors.
    #[arg(long, value_name = "TEXT")]
    context: Option<String>,
    /// Stop after this much trailing silence.
    #[arg(long, value_name = "MS")]
    silence_stop: Option<u64>,
}

fn socket_path(cli: &Cli) -> PathBuf {
    if let Some(p) = &cli.socket {
        return p.clone();
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("ursula.sock"),
        None => {
            eprintln!("error: XDG_RUNTIME_DIR is not set; pass --socket");
            exit(1);
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let _socket = socket_path(&cli);
    match cli.command {
        Command::Serve => todo!(),
        Command::Record(_) => todo!(),
        Command::Stop => todo!(),
        Command::Cancel => todo!(),
        Command::Toggle(_) => todo!(),
        Command::File { .. } => todo!(),
        Command::Status => todo!(),
        Command::Raw => todo!(),
    }
}
