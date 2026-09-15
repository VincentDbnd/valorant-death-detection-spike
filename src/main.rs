//! Spike: prouver qu'on peut capturer une region de l'ecran pendant qu'un jeu
//! tourne en plein ecran exclusif, via Windows.Graphics.Capture.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use parking_lot::Mutex;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler, GraphicsCaptureApiError};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

#[derive(Parser)]
#[command(about = "Capture une frame d'une region de l'ecran (WGC) et l'ecrit en PNG")]
struct Args {
    /// Bord gauche de la region, en % de la largeur de l'ecran
    #[arg(long, default_value_t = 0.0)]
    x: f64,
    /// Bord haut de la region, en % de la hauteur de l'ecran
    #[arg(long, default_value_t = 0.0)]
    y: f64,
    /// Largeur de la region, en % de la largeur de l'ecran
    #[arg(long, default_value_t = 100.0)]
    w: f64,
    /// Hauteur de la region, en % de la hauteur de l'ecran
    #[arg(long, default_value_t = 100.0)]
    h: f64,
    /// Fichier PNG de sortie
    #[arg(long, default_value = "capture.png")]
    out: String,
}

/// Region a capturer, en pixels, dans le repere de la frame.
#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

struct Flags {
    rect: Rect,
    out: String,
    /// Rempli par le handler; lu par le main une fois la capture terminee.
    result: Arc<Mutex<Option<Shot>>>,
}

/// Ce que la capture a effectivement produit.
struct Shot {
    frame_width: u32,
    frame_height: u32,
    rect: Rect,
    mean: f64,
    stddev: f64,
    max: u8,
}

struct Capture {
    flags: Flags,
    done: Arc<AtomicBool>,
}

impl GraphicsCaptureApiHandler for Capture {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, done: Arc::new(AtomicBool::new(false)) })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Une seule frame: les suivantes (s'il en arrive avant l'arret) sont ignorees.
        if self.done.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let (fw, fh) = (frame.width(), frame.height());
        // La frame WGC peut differer legerement de la resolution rapportee par
        // l'ecran (arrondis du compositeur): on reclampe sur ses dimensions reelles.
        let rect = clamp_rect(self.flags.rect, fw, fh)?;

        let buffer = frame.buffer_crop(rect.x, rect.y, rect.x + rect.w, rect.y + rect.h)?;
        let mut scratch = Vec::new();
        let pixels = buffer.as_nopadding_buffer(&mut scratch);

        let (mean, stddev, max) = luminance_stats(pixels);

        let img: image::RgbaImage = image::ImageBuffer::from_raw(rect.w, rect.h, pixels.to_vec())
            .ok_or("taille de buffer inattendue")?;
        img.save_with_format(&self.flags.out, image::ImageFormat::Png)?;

        *self.flags.result.lock() =
            Some(Shot { frame_width: fw, frame_height: fh, rect, mean, stddev, max });

        control.stop();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Convertit un pourcentage en pixels et borne la region a l'ecran.
fn clamp_rect(r: Rect, fw: u32, fh: u32) -> Result<Rect, Box<dyn std::error::Error + Send + Sync>> {
    let x = r.x.min(fw.saturating_sub(1));
    let y = r.y.min(fh.saturating_sub(1));
    let w = r.w.min(fw - x);
    let h = r.h.min(fh - y);
    if w == 0 || h == 0 {
        return Err("region vide apres clamp sur la frame".into());
    }
    Ok(Rect { x, y, w, h })
}

/// Moyenne, ecart-type et maximum de la luminosite (Rec. 601) sur du RGBA8.
fn luminance_stats(pixels: &[u8]) -> (f64, f64, u8) {
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut max = 0u8;
    let n = (pixels.len() / 4) as f64;

    for px in pixels.chunks_exact(4) {
        let l = 0.299 * f64::from(px[0]) + 0.587 * f64::from(px[1]) + 0.114 * f64::from(px[2]);
        sum += l;
        sum_sq += l * l;
        let rounded = l.round().clamp(0.0, 255.0) as u8;
        if rounded > max {
            max = rounded;
        }
    }

    let mean = sum / n;
    let variance = (sum_sq / n - mean * mean).max(0.0);
    (mean, variance.sqrt(), max)
}

fn pct_to_px(pct: f64, total: u32) -> u32 {
    ((pct / 100.0) * f64::from(total)).round().clamp(0.0, f64::from(total)) as u32
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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    for (name, v) in [("--x", args.x), ("--y", args.y), ("--w", args.w), ("--h", args.h)] {
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("{name} doit etre un pourcentage entre 0 et 100 (recu {v})").into());
        }
    }
    if args.w == 0.0 || args.h == 0.0 {
        return Err("--w et --h doivent etre > 0".into());
    }

    match GraphicsCaptureApi::is_supported() {
        Ok(true) => {}
        Ok(false) => {
            return Err("Windows.Graphics.Capture indisponible sur cette machine \
                        (Windows 10 1803+ requis). Capture impossible."
                .into());
        }
        Err(e) => {
            return Err(format!(
                "impossible d'interroger Windows.Graphics.Capture ({e}). \
                 API probablement indisponible."
            )
            .into());
        }
    }

    let monitor = Monitor::primary()
        .map_err(|e| format!("aucun ecran principal detecte ({e})"))?;
    let (mw, mh) = (monitor.width()?, monitor.height()?);

    let rect = Rect {
        x: pct_to_px(args.x, mw),
        y: pct_to_px(args.y, mh),
        w: pct_to_px(args.w, mw).max(1),
        h: pct_to_px(args.h, mh).max(1),
    };

    println!("Ecran detecte      : {mw}x{mh}");
    println!(
        "Region demandee    : x={:.1}% y={:.1}% w={:.1}% h={:.1}%",
        args.x, args.y, args.w, args.h
    );
    println!(
        "Region en pixels   : x={} y={} w={} h={}  (soit {}..{} x {}..{})",
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        rect.x,
        rect.x + rect.w,
        rect.y,
        rect.y + rect.h
    );

    let result = Arc::new(Mutex::new(None));
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        Flags { rect, out: args.out.clone(), result: result.clone() },
    );

    Capture::start(settings).map_err(describe_capture_error)?;

    let shot = result
        .lock()
        .take()
        .ok_or("aucune frame n'a ete livree par Windows.Graphics.Capture")?;

    if shot.frame_width != mw || shot.frame_height != mh {
        println!(
            "Note: la frame WGC fait {}x{}, region reclampee a x={} y={} w={} h={}",
            shot.frame_width, shot.frame_height, shot.rect.x, shot.rect.y, shot.rect.w, shot.rect.h
        );
    }

    println!("PNG ecrit          : {}", args.out);
    println!(
        "Luminosite         : moyenne={:.2} ecart-type={:.2} max={}",
        shot.mean, shot.stddev, shot.max
    );

    if shot.max == 0 {
        println!(
            "VERDICT: IMAGE UNIFORMEMENT NOIRE (tous les pixels a 0). \
             C'est le symptome d'un echec de capture en plein ecran exclusif."
        );
    } else if shot.stddev < 1.0 {
        println!(
            "VERDICT: image quasi uniforme (ecart-type {:.2}). Contenu suspect, a verifier.",
            shot.stddev
        );
    } else {
        println!("VERDICT: image non noire, la capture contient du contenu reel.");
    }

    Ok(())
}

/// Traduit les erreurs de la pile WGC en messages exploitables.
fn describe_capture_error(
    e: GraphicsCaptureApiError<Box<dyn std::error::Error + Send + Sync>>,
) -> String {
    use windows_capture::graphics_capture_api::Error as ApiError;

    match e {
        GraphicsCaptureApiError::GraphicsCaptureApiError(ApiError::Unsupported) => {
            "Windows.Graphics.Capture n'est pas supporte sur cette machine.".to_string()
        }
        GraphicsCaptureApiError::ItemConvertFailed => {
            "capture refusee: impossible de creer l'element de capture pour cet ecran \
             (autorisation de capture d'ecran refusee ou ecran inaccessible)."
                .to_string()
        }
        GraphicsCaptureApiError::FailedToInitWinRT => {
            "impossible d'initialiser WinRT: l'API de capture est indisponible.".to_string()
        }
        other => format!("echec de la capture: {other}"),
    }
}
