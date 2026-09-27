//! Spike de detection de mort: capture et mesure (Windows), machine a etats
//! vivant/mort et rejeu des mesures (toutes plateformes).

#[cfg(windows)]
mod capture;
mod detector;
mod replay;

use clap::{Parser, Subcommand};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser)]
#[command(about = "Capture WGC d'une region de la zone de jeu, mesure NCC d'un template, rejeu vivant/mort")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    #[cfg(windows)]
    #[command(flatten)]
    Capture(capture::CaptureCmd),
    /// Rejoue des CSV de `watch` a travers la machine a etats vivant/mort.
    Replay(replay::ReplayArgs),
    /// Nombre de transitions pour chaque combinaison seuil / frames_to_dead /
    /// frames_to_alive.
    Sweep(replay::SweepArgs),
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERREUR: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), BoxError> {
    match &Cli::parse().cmd {
        #[cfg(windows)]
        Cmd::Capture(c) => capture::run(c),
        Cmd::Replay(a) => replay::cmd_replay(a),
        Cmd::Sweep(a) => replay::cmd_sweep(a),
    }
}
