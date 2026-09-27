//! Sous-commandes replay et sweep: rejouent des CSV de `watch` a travers la
//! machine a etats. Aucun acces a l'ecran ni a Windows.

use std::path::{Path, PathBuf};

use clap::Args;

use crate::BoxError;
use crate::detector::{Config, Detector, Frame, Freeze, State};

#[derive(Args)]
pub struct ReplayArgs {
    /// CSV produit par `watch --log` (repetable: --csv a.csv --csv b.csv, ou --csv a.csv b.csv)
    #[arg(long, required = true, num_args = 1..)]
    csv: Vec<PathBuf>,
    /// score > seuil => candidat Dead
    #[arg(long, default_value_t = Config::default().threshold)]
    threshold: f32,
    /// Frames consecutives pour basculer Alive -> Dead
    #[arg(long, default_value_t = Config::default().frames_to_dead,
          value_parser = clap::value_parser!(u32).range(1..))]
    frames_to_dead: u32,
    /// Frames consecutives pour basculer Dead -> Alive
    #[arg(long, default_value_t = Config::default().frames_to_alive,
          value_parser = clap::value_parser!(u32).range(1..))]
    frames_to_alive: u32,
    /// Une ligne par frame
    #[arg(long)]
    verbose: bool,
}

#[derive(Args)]
pub struct SweepArgs {
    /// CSV produit par `watch --log` (plusieurs acceptes)
    #[arg(long, required = true, num_args = 1..)]
    csv: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Lecture CSV
// ---------------------------------------------------------------------------

struct Row {
    frame: Frame,
    t_s: f64,
}

struct Session {
    name: String,
    rows: Vec<Row>,
    /// Lignes sans valeur foreground (colonne absente ou vide), supposees 1.
    missing_foreground: usize,
    /// Lignes sans valeur capture_ok, supposees 1.
    missing_capture_ok: usize,
}

fn load(path: &Path) -> Result<Session, BoxError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("lecture de {} impossible: {e}", path.display()))?;
    let mut lines = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty());
    let (_, header) = lines.next().ok_or_else(|| format!("{}: fichier vide", path.display()))?;
    let cols: Vec<&str> = header.split(',').map(str::trim).collect();
    let col = |name: &str| cols.iter().position(|c| *c == name);
    let need = |name: &str| col(name).ok_or_else(|| format!("{}: colonne {name:?} absente", path.display()));
    let (i_ms, i_t, i_score) = (need("unix_ms")?, need("t_s")?, need("score")?);
    let (i_fg, i_ok) = (col("foreground"), col("capture_ok"));

    let mut s = Session {
        name: path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into()),
        rows: Vec::new(),
        missing_foreground: 0,
        missing_capture_ok: 0,
    };
    for (n, line) in lines {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let err = |what: &str| format!("{}:{}: {what} invalide: {line:?}", path.display(), n + 1);
        let get = |i: usize| f.get(i).copied().unwrap_or("");
        // Colonne optionnelle 0/1; absente ou vide => None.
        let flag = |i: Option<usize>| -> Result<Option<bool>, String> {
            match i.map(get).unwrap_or("") {
                "" => Ok(None),
                "0" => Ok(Some(false)),
                "1" => Ok(Some(true)),
                _ => Err(err("booleen")),
            }
        };
        let fg = flag(i_fg)?;
        let ok = flag(i_ok)?;
        s.missing_foreground += usize::from(fg.is_none());
        s.missing_capture_ok += usize::from(ok.is_none());
        s.rows.push(Row {
            frame: Frame {
                timestamp_ms: get(i_ms).parse().map_err(|_| err("unix_ms"))?,
                score: get(i_score).parse().map_err(|_| err("score"))?,
                foreground: fg.unwrap_or(true),
                capture_ok: ok.unwrap_or(true),
            },
            t_s: get(i_t).parse().map_err(|_| err("t_s"))?,
        });
    }
    if s.rows.is_empty() {
        return Err(format!("{}: aucune mesure", path.display()).into());
    }
    Ok(s)
}

fn load_all(paths: &[PathBuf]) -> Result<Vec<Session>, BoxError> {
    paths.iter().map(|p| load(p)).collect()
}

// ---------------------------------------------------------------------------
// Rejeu
// ---------------------------------------------------------------------------

struct Transition {
    unix_ms: u64,
    t_s: f64,
    from: State,
    to: State,
}

#[derive(Default)]
struct Stats {
    transitions: Vec<Transition>,
    alive_ms: u64,
    dead_ms: u64,
    frozen_background: usize,
    frozen_zero: usize,
    capture_ko: usize,
}

/// Rejoue une session depuis l'etat initial. `on_frame` recoit chaque frame
/// avec la sortie de la machine et le compteur d'hysteresis apres coup.
fn replay(s: &Session, cfg: Config, mut on_frame: impl FnMut(&Row, &crate::detector::Output, u32)) -> Stats {
    let mut d = Detector::new(cfg);
    let mut st = Stats::default();
    let mut prev: Option<(u64, State)> = None;
    for row in &s.rows {
        let f = &row.frame;
        // L'intervalle depuis la frame precedente compte dans l'etat
        // qu'avait la machine pendant cet intervalle (gel compris).
        if let Some((ts, state)) = prev {
            let dt = f.timestamp_ms.saturating_sub(ts);
            match state {
                State::Alive => st.alive_ms += dt,
                State::Dead => st.dead_ms += dt,
            }
        }
        let before = d.state();
        let out = d.push(f);
        on_frame(row, &out, d.streak());
        match out.frozen {
            Some(Freeze::Background) => st.frozen_background += 1,
            Some(Freeze::ZeroScore) => st.frozen_zero += 1,
            None => {}
        }
        st.capture_ko += usize::from(!f.capture_ok);
        if out.changed {
            st.transitions.push(Transition { unix_ms: f.timestamp_ms, t_s: row.t_s, from: before, to: out.state });
        }
        prev = Some((f.timestamp_ms, out.state));
    }
    st
}

fn secs(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { 100.0 * part as f64 / total as f64 }
}

fn print_stats(st: &Stats, frames: usize) {
    let total = st.alive_ms + st.dead_ms;
    let frozen = st.frozen_background + st.frozen_zero;
    println!("Transitions        : {}", st.transitions.len());
    println!("Duree Alive        : {:9.1} s ({:5.1} %)", secs(st.alive_ms), pct(st.alive_ms, total));
    println!("Duree Dead         : {:9.1} s ({:5.1} %)", secs(st.dead_ms), pct(st.dead_ms, total));
    println!(
        "Frames gelees      : {frozen} / {frames} (foreground=0: {}, score=0: {})",
        st.frozen_background, st.frozen_zero
    );
    println!("capture_ok=0       : {} (information seule, non exploite par la machine)", st.capture_ko);
}

pub fn cmd_replay(a: &ReplayArgs) -> Result<(), BoxError> {
    if !a.threshold.is_finite() {
        return Err(format!("--threshold invalide (recu {})", a.threshold).into());
    }
    let cfg = Config { threshold: a.threshold, frames_to_dead: a.frames_to_dead, frames_to_alive: a.frames_to_alive };
    let sessions = load_all(&a.csv)?;
    println!(
        "Parametres: seuil={} frames_to_dead={} frames_to_alive={}",
        cfg.threshold, cfg.frames_to_dead, cfg.frames_to_alive
    );

    let mut all = Stats::default();
    let mut all_frames = 0;
    for s in &sessions {
        println!();
        let last = s.rows.last().unwrap();
        println!("=== {} : {} frames, {:.1} s", s.name, s.rows.len(), last.t_s - s.rows[0].t_s);
        for (what, n) in [("foreground", s.missing_foreground), ("capture_ok", s.missing_capture_ok)] {
            if n > 0 {
                println!("Note: {what} absent sur {n} lignes, suppose 1");
            }
        }

        let st = replay(s, cfg, |row, out, streak| {
            if a.verbose {
                let f = &row.frame;
                let verdict = match out.frozen {
                    Some(Freeze::Background) => "GEL fg=0",
                    Some(Freeze::ZeroScore) => "GEL score=0",
                    None if f.score > cfg.threshold => "cand Dead",
                    None => "cand Alive",
                };
                println!(
                    "  {} t={:9.3}s score={:+.6} fg={} ok={} {verdict:<11} serie={streak:<2} {:?}{}",
                    f.timestamp_ms,
                    row.t_s,
                    f.score,
                    u8::from(f.foreground),
                    u8::from(f.capture_ok),
                    out.state,
                    if out.changed { "  <<< BASCULE" } else { "" }
                );
            }
        });

        for tr in &st.transitions {
            println!("  {} t={:9.3}s  {:?} -> {:?}", tr.unix_ms, tr.t_s, tr.from, tr.to);
        }
        print_stats(&st, s.rows.len());

        all_frames += s.rows.len();
        all.alive_ms += st.alive_ms;
        all.dead_ms += st.dead_ms;
        all.frozen_background += st.frozen_background;
        all.frozen_zero += st.frozen_zero;
        all.capture_ko += st.capture_ko;
        all.transitions.extend(st.transitions);
    }

    if sessions.len() > 1 {
        println!();
        println!("=== Total ({} sessions, rejouees chacune depuis Alive)", sessions.len());
        print_stats(&all, all_frames);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Balayage
// ---------------------------------------------------------------------------

const FRAMES_RANGE: std::ops::RangeInclusive<u32> = 1..=10;

/// Seuils 0.35 a 0.55 par pas de 0.05, calcules en centiemes pour eviter la
/// derive d'une addition repetee.
fn thresholds() -> impl Iterator<Item = f32> {
    (35..=55).step_by(5).map(|c| c as f32 / 100.0)
}

/// Intervalle median entre deux mesures, pour traduire des frames en ms.
fn median_period_ms(sessions: &[Session]) -> Option<u64> {
    let mut dts: Vec<u64> = sessions
        .iter()
        .flat_map(|s| s.rows.windows(2).map(|w| w[1].frame.timestamp_ms.saturating_sub(w[0].frame.timestamp_ms)))
        .filter(|&dt| dt > 0)
        .collect();
    dts.sort_unstable();
    dts.get(dts.len() / 2).copied()
}

pub fn cmd_sweep(a: &SweepArgs) -> Result<(), BoxError> {
    let sessions = load_all(&a.csv)?;
    let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
    let period = median_period_ms(&sessions);
    println!("Sessions : {} (chacune rejouee depuis Alive, transitions additionnees)", names.join(", "));
    match period {
        Some(p) => println!("Periode mediane entre mesures : {p} ms (latence = frames x periode)"),
        None => println!("Periode mediane entre mesures : inconnue"),
    }
    let latency = |n: u32| period.map_or_else(|| "?".into(), |p| format!("{}ms", u64::from(n) * p));

    for threshold in thresholds() {
        println!();
        println!("Seuil {threshold:.2} : nombre de transitions (lignes: frames_to_dead, colonnes: frames_to_alive)");
        print!("{:>22} |", "frames_to_alive");
        for fta in FRAMES_RANGE {
            print!("{fta:>7}");
        }
        println!();
        print!("{:>22} |", "latence Dead->Alive");
        for fta in FRAMES_RANGE {
            print!("{:>7}", latency(fta));
        }
        println!();
        println!("{}", "-".repeat(24 + 7 * FRAMES_RANGE.count()));
        for ftd in FRAMES_RANGE {
            print!("{:>22} |", format!("ftd={ftd:<2} ({})", latency(ftd)));
            for fta in FRAMES_RANGE {
                let cfg = Config { threshold, frames_to_dead: ftd, frames_to_alive: fta };
                let n: usize = sessions.iter().map(|s| replay(s, cfg, |_, _, _| {}).transitions.len()).sum();
                print!("{n:>7}");
            }
            println!();
        }
    }
    Ok(())
}
