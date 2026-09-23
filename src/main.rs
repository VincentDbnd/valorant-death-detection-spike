//! Spike: capture d'une region de l'ecran pendant qu'un jeu tourne en plein
//! ecran exclusif (Windows.Graphics.Capture), puis mesure brute de la
//! detectabilite d'une icone par correlation croisee normalisee (NCC).
//!
//! Aucune logique de decision ici: uniquement de la mesure.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use parking_lot::Mutex;
use windows_capture::capture::{
    CaptureControl, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser)]
#[command(about = "Capture WGC d'une region de la zone de jeu et mesure NCC d'un template")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Capture une frame de la region en PNG couleur + stats de luminosite
    /// (test "l'image est-elle noire ?").
    Snap {
        #[command(flatten)]
        region: RegionArgs,
        /// Fichier PNG de sortie
        #[arg(long, default_value = "capture.png")]
        out: PathBuf,
        /// Delai avant la capture, en secondes (le temps de basculer sur le jeu)
        #[arg(long, default_value_t = 0)]
        delay: u64,
    },
    /// Decoupe la region et l'enregistre comme template (PNG niveaux de gris).
    Extract {
        #[command(flatten)]
        region: RegionArgs,
        /// Fichier PNG du template produit
        #[arg(long)]
        out: PathBuf,
        /// Decoupe dans ce PNG au lieu de capturer l'ecran
        #[arg(long)]
        from: Option<PathBuf>,
        /// Delai avant la capture, en secondes (ignore avec --from)
        #[arg(long, default_value_t = 0)]
        delay: u64,
    },
    /// Mesure en boucle le score NCC du template dans la region de recherche.
    Watch {
        #[command(flatten)]
        region: RegionArgs,
        /// Template PNG (converti en niveaux de gris s'il est en couleur)
        #[arg(long)]
        template: PathBuf,
        /// Frequence de mesure, en Hz
        #[arg(long, default_value_t = 10.0)]
        hz: f64,
        /// Ecrit aussi chaque mesure dans ce fichier CSV
        #[arg(long)]
        log: Option<PathBuf>,
        /// Mesure une seule fois sur ce PNG au lieu de capturer l'ecran
        #[arg(long)]
        from: Option<PathBuf>,
    },
}

#[derive(Args, Clone)]
struct RegionArgs {
    /// Bord gauche de la region, en % de la largeur de la zone de jeu
    #[arg(long, default_value_t = 0.0)]
    x: f64,
    /// Bord haut de la region, en % de la hauteur de la zone de jeu
    #[arg(long, default_value_t = 0.0)]
    y: f64,
    /// Largeur de la region, en % de la largeur de la zone de jeu
    #[arg(long, default_value_t = 100.0)]
    w: f64,
    /// Hauteur de la region, en % de la hauteur de la zone de jeu
    #[arg(long, default_value_t = 100.0)]
    h: f64,
    /// Applique les pourcentages a la surface brute (pas de recadrage 16:9)
    #[arg(long)]
    no_letterbox: bool,
    /// Capture la fenetre dont le titre est donne (exact, sinon sous-chaine)
    /// au lieu du moniteur principal
    #[arg(long)]
    window: Option<String>,
}

impl RegionArgs {
    fn validate(&self) -> Result<(), BoxError> {
        for (name, v) in [("--x", self.x), ("--y", self.y), ("--w", self.w), ("--h", self.h)] {
            if !(0.0..=100.0).contains(&v) {
                return Err(format!("{name} doit etre un pourcentage entre 0 et 100 (recu {v})").into());
            }
        }
        if self.w == 0.0 || self.h == 0.0 {
            return Err("--w et --h doivent etre > 0".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Geometrie: surface capturee -> zone de jeu utile -> region demandee
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[derive(Clone, Copy, PartialEq)]
struct RegionSpec {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    letterbox: bool,
}

impl From<&RegionArgs> for RegionSpec {
    fn from(a: &RegionArgs) -> Self {
        Self { x: a.x, y: a.y, w: a.w, h: a.h, letterbox: !a.no_letterbox }
    }
}

/// Ou tombe la region demandee dans une surface donnee.
#[derive(Clone, Copy, PartialEq)]
struct Geometry {
    surface_w: u32,
    surface_h: u32,
    /// Zone de jeu utile, dans le repere de la surface.
    zone: Rect,
    /// Region demandee, dans le repere de la surface.
    region: Rect,
    letterbox_requested: bool,
}

fn pct_to_px(pct: f64, total: u32) -> u32 {
    ((pct / 100.0) * f64::from(total)).round().clamp(0.0, f64::from(total)) as u32
}

/// Deduit la zone 16:9 centree a pleine hauteur (si la surface est plus large
/// que 16:9), puis y place la region exprimee en pourcentages.
fn geometry(spec: &RegionSpec, sw: u32, sh: u32) -> Result<Geometry, String> {
    if sw == 0 || sh == 0 {
        return Err(format!("surface vide ({sw}x{sh})"));
    }
    let wider_than_16_9 = u64::from(sw) * 9 > u64::from(sh) * 16;
    let zone = if spec.letterbox && wider_than_16_9 {
        let zw = ((f64::from(sh) * 16.0 / 9.0).round() as u32).min(sw);
        Rect { x: (sw - zw) / 2, y: 0, w: zw, h: sh }
    } else {
        Rect { x: 0, y: 0, w: sw, h: sh }
    };

    let rx = pct_to_px(spec.x, zone.w).min(zone.w - 1);
    let ry = pct_to_px(spec.y, zone.h).min(zone.h - 1);
    let rw = pct_to_px(spec.w, zone.w).max(1).min(zone.w - rx);
    let rh = pct_to_px(spec.h, zone.h).max(1).min(zone.h - ry);

    Ok(Geometry {
        surface_w: sw,
        surface_h: sh,
        zone,
        region: Rect { x: zone.x + rx, y: zone.y + ry, w: rw, h: rh },
        letterbox_requested: spec.letterbox,
    })
}

fn print_geometry(g: &Geometry) {
    let z = g.zone;
    let r = g.region;
    let why = if !g.letterbox_requested {
        "surface entiere (--no-letterbox)"
    } else if z.w == g.surface_w {
        "surface entiere (deja 16:9 ou plus etroite)"
    } else {
        "16:9 centree, pleine hauteur"
    };
    println!("Surface capturee   : {}x{}", g.surface_w, g.surface_h);
    println!("Zone utile         : {}x{}  offset x={} y={}  [{why}]", z.w, z.h, z.x, z.y);
    println!(
        "Region (surface)   : x={} y={} w={} h={}  (soit {}..{} x {}..{})",
        r.x,
        r.y,
        r.w,
        r.h,
        r.x,
        r.x + r.w,
        r.y,
        r.y + r.h
    );
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Image 8 bits en niveaux de gris.
struct Gray {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

/// Luminance Rec. 601 en entier (sur RGBA8). Pour un pixel deja gris
/// (r=g=b=v) elle redonne exactement v, donc recharger un template gris est
/// sans perte.
fn rgba_to_gray(w: u32, h: u32, rgba: &[u8]) -> Gray {
    let px = rgba
        .chunks_exact(4)
        .map(|p| ((77 * u32::from(p[0]) + 150 * u32::from(p[1]) + 29 * u32::from(p[2]) + 128) >> 8) as u8)
        .collect();
    Gray { w, h, px }
}

fn load_rgba(path: &Path) -> Result<image::RgbaImage, BoxError> {
    Ok(image::open(path)
        .map_err(|e| format!("lecture de {} impossible: {e}", path.display()))?
        .to_rgba8())
}

/// Decoupe `r` dans une image RGBA pleine surface.
fn crop_rgba(img: &image::RgbaImage, r: Rect) -> Vec<u8> {
    image::imageops::crop_imm(img, r.x, r.y, r.w, r.h).to_image().into_raw()
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

// ---------------------------------------------------------------------------
// NCC
// ---------------------------------------------------------------------------

/// Template pre-centre: t'(i) = t(i) - moyenne(t), et sa norme sqrt(sum t'^2).
struct Template {
    w: u32,
    h: u32,
    centered: Vec<f32>,
    norm: f64,
}

impl Template {
    fn new(g: &Gray) -> Result<Self, BoxError> {
        let n = g.px.len() as f64;
        let mean = g.px.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
        let centered: Vec<f32> = g.px.iter().map(|&v| (f64::from(v) - mean) as f32).collect();
        let norm = centered.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>().sqrt();
        if norm < 1e-6 {
            return Err("template uniforme (variance nulle): NCC indefinie".into());
        }
        Ok(Self { w: g.w, h: g.h, centered, norm })
    }
}

struct Match {
    score: f64,
    /// Coin haut-gauche du meilleur placement, dans le repere de la region.
    x: u32,
    y: u32,
}

/// NCC "zero-mean" exhaustive du template sur toutes les positions de la
/// recherche. Comme sum(t') = 0, le numerateur se reduit a sum(s * t'); la
/// variance locale de s vient d'images integrales (somme et somme des carres).
/// Une fenetre de recherche uniforme (variance nulle) donne un score de 0.
fn ncc(tpl: &Template, s: &Gray) -> Match {
    let (sw, sh) = (s.w as usize, s.h as usize);
    let (tw, th) = (tpl.w as usize, tpl.h as usize);
    let n = (tw * th) as f64;

    // Images integrales (sw+1)x(sh+1), en entiers exacts.
    let iw = sw + 1;
    let mut sum = vec![0u64; iw * (sh + 1)];
    let mut sq = vec![0u64; iw * (sh + 1)];
    for y in 0..sh {
        let (mut row_s, mut row_q) = (0u64, 0u64);
        for x in 0..sw {
            let v = u64::from(s.px[y * sw + x]);
            row_s += v;
            row_q += v * v;
            sum[(y + 1) * iw + x + 1] = sum[y * iw + x + 1] + row_s;
            sq[(y + 1) * iw + x + 1] = sq[y * iw + x + 1] + row_q;
        }
    }
    let rect = |t: &[u64], x: usize, y: usize| {
        t[(y + th) * iw + x + tw] + t[y * iw + x] - t[y * iw + x + tw] - t[(y + th) * iw + x]
    };

    let sf: Vec<f32> = s.px.iter().map(|&v| f32::from(v)).collect();

    let mut best = Match { score: f64::NEG_INFINITY, x: 0, y: 0 };
    for v in 0..=(sh - th) {
        for u in 0..=(sw - tw) {
            let s_sum = rect(&sum, u, v) as f64;
            let s_sq = rect(&sq, u, v) as f64;
            let var_n = s_sq - s_sum * s_sum / n;

            let score = if var_n <= 1e-9 {
                0.0
            } else {
                let mut num = 0.0f64;
                for j in 0..th {
                    let srow = &sf[(v + j) * sw + u..(v + j) * sw + u + tw];
                    let trow = &tpl.centered[j * tw..(j + 1) * tw];
                    num += f64::from(dot(srow, trow));
                }
                num / (var_n.sqrt() * tpl.norm)
            };

            if score > best.score {
                best = Match { score, x: u as u32, y: v as u32 };
            }
        }
    }
    best
}

/// Produit scalaire sur 8 accumulateurs, pour que le compilateur vectorise.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let ca = a.chunks_exact(8);
    let cb = b.chunks_exact(8);
    let tail: f32 = ca.remainder().iter().zip(cb.remainder()).map(|(x, y)| x * y).sum();
    for (x, y) in ca.zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum::<f32>() + tail
}

fn check_fits(tpl: &Template, g: &Geometry) -> Result<(), BoxError> {
    if g.region.w < tpl.w || g.region.h < tpl.h {
        return Err(format!(
            "region de recherche {}x{} plus petite que le template {}x{}",
            g.region.w, g.region.h, tpl.w, tpl.h
        )
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Capture WGC
// ---------------------------------------------------------------------------

/// Derniere region capturee, deposee par le handler et lue par le main.
struct Grab {
    geom: Geometry,
    rgba: Vec<u8>,
    /// Numero de frame WGC (WGC n'envoie une frame que si l'ecran change).
    seq: u64,
}

struct Flags {
    spec: RegionSpec,
    /// Arrete la capture apres la premiere frame.
    once: bool,
    latest: Arc<Mutex<Option<Grab>>>,
}

struct Grabber {
    flags: Flags,
    seq: u64,
    geom: Option<Geometry>,
}

impl GraphicsCaptureApiHandler for Grabber {
    type Flags = Flags;
    type Error = BoxError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, seq: 0, geom: None })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.once && self.seq > 0 {
            return Ok(());
        }
        self.seq += 1;

        // La geometrie est recalculee si la surface change de taille
        // (fenetre redimensionnee).
        let (fw, fh) = (frame.width(), frame.height());
        let geom = match self.geom {
            Some(g) if g.surface_w == fw && g.surface_h == fh => g,
            _ => geometry(&self.flags.spec, fw, fh)?,
        };
        self.geom = Some(geom);

        let r = geom.region;
        let buffer = frame.buffer_crop(r.x, r.y, r.x + r.w, r.y + r.h)?;
        let mut scratch = Vec::new();
        let rgba = buffer.as_nopadding_buffer(&mut scratch).to_vec();

        *self.flags.latest.lock() = Some(Grab { geom, rgba, seq: self.seq });

        if self.flags.once {
            control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

enum Target {
    Monitor(Monitor),
    Window(Window),
}

fn check_wgc() -> Result<(), BoxError> {
    match GraphicsCaptureApi::is_supported() {
        Ok(true) => Ok(()),
        Ok(false) => Err("Windows.Graphics.Capture indisponible sur cette machine \
                          (Windows 10 1803+ requis). Capture impossible."
            .into()),
        Err(e) => Err(format!(
            "impossible d'interroger Windows.Graphics.Capture ({e}). API probablement indisponible."
        )
        .into()),
    }
}

fn resolve_target(window: Option<&str>) -> Result<Target, BoxError> {
    check_wgc()?;
    match window {
        None => {
            let m = Monitor::primary().map_err(|e| format!("aucun ecran principal detecte ({e})"))?;
            println!("Source             : moniteur principal");
            Ok(Target::Monitor(m))
        }
        Some(title) => {
            let w = Window::from_name(title)
                .or_else(|_| Window::from_contains_name(title))
                .map_err(|e| format!("aucune fenetre ne correspond a {title:?} ({e})"))?;
            println!(
                "Source             : fenetre {:?} (processus {})",
                w.title().unwrap_or_default(),
                w.process_name().unwrap_or_else(|_| "?".into())
            );
            Ok(Target::Window(w))
        }
    }
}

fn settings<T: TryInto<GraphicsCaptureItemType>>(item: T, flags: Flags) -> Settings<Flags, T> {
    Settings::new(
        item,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        flags,
    )
}

/// Capture une seule frame (bloquant).
fn capture_once(target: Target, spec: RegionSpec) -> Result<Grab, BoxError> {
    let latest = Arc::new(Mutex::new(None));
    let flags = Flags { spec, once: true, latest: latest.clone() };
    match target {
        Target::Monitor(m) => Grabber::start(settings(m, flags)),
        Target::Window(w) => Grabber::start(settings(w, flags)),
    }
    .map_err(describe_capture_error)?;
    let grab = latest.lock().take();
    grab.ok_or_else(|| "aucune frame n'a ete livree par Windows.Graphics.Capture".into())
}

/// Demarre une capture continue sur un thread dedie.
fn capture_start(
    target: Target,
    spec: RegionSpec,
    latest: Arc<Mutex<Option<Grab>>>,
) -> Result<CaptureControl<Grabber, BoxError>, BoxError> {
    let flags = Flags { spec, once: false, latest };
    Ok(match target {
        Target::Monitor(m) => Grabber::start_free_threaded(settings(m, flags)),
        Target::Window(w) => Grabber::start_free_threaded(settings(w, flags)),
    }
    .map_err(describe_capture_error)?)
}

/// Traduit les erreurs de la pile WGC en messages exploitables.
fn describe_capture_error(e: GraphicsCaptureApiError<BoxError>) -> String {
    use windows_capture::graphics_capture_api::Error as ApiError;

    match e {
        GraphicsCaptureApiError::GraphicsCaptureApiError(ApiError::Unsupported) => {
            "Windows.Graphics.Capture n'est pas supporte sur cette machine.".to_string()
        }
        GraphicsCaptureApiError::ItemConvertFailed => {
            "capture refusee: impossible de creer l'element de capture pour cette source \
             (autorisation de capture refusee, ecran ou fenetre inaccessible)."
                .to_string()
        }
        GraphicsCaptureApiError::FailedToInitWinRT => {
            "impossible d'initialiser WinRT: l'API de capture est indisponible.".to_string()
        }
        other => format!("echec de la capture: {other}"),
    }
}

/// Compte a rebours sur une seule ligne, pour laisser le temps de basculer sur le jeu.
fn countdown(secs: u64) {
    println!("Capture dans {secs}s - bascule sur le jeu maintenant.");
    for remaining in (1..=secs).rev() {
        print!("\r  {remaining:>3}s ");
        let _ = std::io::stdout().flush();
        thread::sleep(Duration::from_secs(1));
    }
    // La ligne du compte a rebours est reecrite pour ne pas polluer la sortie finale.
    print!("\r        \r");
    let _ = std::io::stdout().flush();
    println!("Capture...");
}

/// Region demandee, soit capturee a l'ecran, soit decoupee dans un PNG.
fn grab_region(
    region: &RegionArgs,
    from: Option<&Path>,
    delay: u64,
) -> Result<(Geometry, Vec<u8>), BoxError> {
    let spec = RegionSpec::from(region);
    match from {
        Some(path) => {
            if region.window.is_some() {
                println!("Note: --window ignore avec --from");
            }
            println!("Source             : fichier {}", path.display());
            let img = load_rgba(path)?;
            let geom = geometry(&spec, img.width(), img.height())?;
            print_geometry(&geom);
            Ok((geom, crop_rgba(&img, geom.region)))
        }
        None => {
            let target = resolve_target(region.window.as_deref())?;
            if delay > 0 {
                countdown(delay);
            }
            let grab = capture_once(target, spec)?;
            print_geometry(&grab.geom);
            Ok((grab.geom, grab.rgba))
        }
    }
}

// ---------------------------------------------------------------------------
// Sous-commandes
// ---------------------------------------------------------------------------

fn cmd_snap(region: &RegionArgs, out: &Path, delay: u64) -> Result<(), BoxError> {
    let (geom, rgba) = grab_region(region, None, delay)?;
    let r = geom.region;
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(r.w, r.h, rgba).ok_or("taille de buffer inattendue")?;
    img.save_with_format(out, image::ImageFormat::Png)?;

    let (mean, stddev, max) = luminance_stats(img.as_raw());
    println!("PNG ecrit          : {}", out.display());
    println!("Luminosite         : moyenne={mean:.2} ecart-type={stddev:.2} max={max}");
    if max == 0 {
        println!(
            "VERDICT: IMAGE UNIFORMEMENT NOIRE (tous les pixels a 0). \
             C'est le symptome d'un echec de capture en plein ecran exclusif."
        );
    } else if stddev < 1.0 {
        println!("VERDICT: image quasi uniforme (ecart-type {stddev:.2}). Contenu suspect, a verifier.");
    } else {
        println!("VERDICT: image non noire, la capture contient du contenu reel.");
    }
    Ok(())
}

fn cmd_extract(region: &RegionArgs, out: &Path, from: Option<&Path>, delay: u64) -> Result<(), BoxError> {
    let (geom, rgba) = grab_region(region, from, delay)?;
    let r = geom.region;
    let gray = rgba_to_gray(r.w, r.h, &rgba);
    let img: image::GrayImage =
        image::ImageBuffer::from_raw(gray.w, gray.h, gray.px).ok_or("taille de buffer inattendue")?;
    img.save_with_format(out, image::ImageFormat::Png)?;
    println!("Template ecrit     : {} ({}x{} px, niveaux de gris)", out.display(), r.w, r.h);
    Ok(())
}

struct Csv(BufWriter<File>);

impl Csv {
    fn create(path: &Path) -> Result<Self, BoxError> {
        let f = File::create(path).map_err(|e| format!("creation de {} impossible: {e}", path.display()))?;
        let mut w = BufWriter::new(f);
        writeln!(w, "unix_ms,t_s,frame,score,x,y,abs_x,abs_y,compute_ms")?;
        Ok(Self(w))
    }
}

/// Une ligne de mesure, affichee et eventuellement ecrite en CSV.
fn report(
    csv: &mut Option<Csv>,
    t: f64,
    frame: u64,
    m: &Match,
    geom: &Geometry,
    compute: Duration,
) -> Result<(), BoxError> {
    let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let (ax, ay) = (geom.region.x + m.x, geom.region.y + m.y);
    let ms = compute.as_secs_f64() * 1000.0;
    println!(
        "t={t:9.3}s frame={frame:<6} score={:+.4} pos=({},{}) surface=({ax},{ay}) calcul={ms:.2}ms",
        m.score, m.x, m.y
    );
    if let Some(Csv(w)) = csv {
        writeln!(w, "{unix_ms},{t:.3},{frame},{:.6},{},{},{ax},{ay},{ms:.3}", m.score, m.x, m.y)?;
        w.flush()?;
    }
    Ok(())
}

fn measure(tpl: &Template, geom: &Geometry, rgba: &[u8]) -> (Match, Duration) {
    let start = Instant::now();
    let gray = rgba_to_gray(geom.region.w, geom.region.h, rgba);
    let m = ncc(tpl, &gray);
    (m, start.elapsed())
}

fn cmd_watch(
    region: &RegionArgs,
    template: &Path,
    hz: f64,
    log: Option<&Path>,
    from: Option<&Path>,
) -> Result<(), BoxError> {
    if !(hz > 0.0 && hz <= 1000.0) {
        return Err(format!("--hz doit etre dans ]0, 1000] (recu {hz})").into());
    }
    let tpl_rgba = load_rgba(template)?;
    let tpl = Template::new(&rgba_to_gray(tpl_rgba.width(), tpl_rgba.height(), tpl_rgba.as_raw()))?;
    println!("Template           : {} ({}x{} px)", template.display(), tpl.w, tpl.h);

    let mut csv = log.map(Csv::create).transpose()?;

    if let Some(path) = from {
        let (geom, rgba) = grab_region(region, Some(path), 0)?;
        check_fits(&tpl, &geom)?;
        let (m, compute) = measure(&tpl, &geom, &rgba);
        report(&mut csv, 0.0, 0, &m, &geom, compute)?;
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))?;
    }

    let target = resolve_target(region.window.as_deref())?;
    let latest = Arc::new(Mutex::new(None));
    let control = capture_start(target, RegionSpec::from(region), latest.clone())?;
    println!("Mesure a {hz} Hz. Ctrl-C pour sortir.");

    let period = Duration::from_secs_f64(1.0 / hz);
    let t0 = Instant::now();
    let mut next = t0;
    let mut current: Option<Grab> = None;
    let mut shown_geom: Option<Geometry> = None;
    let (mut total, mut count) = (Duration::ZERO, 0u32);
    let mut result = Ok(());

    while !stop.load(Ordering::SeqCst) {
        if control.is_finished() {
            break;
        }
        if let Some(g) = latest.lock().take() {
            current = Some(g);
        }
        if let Some(g) = &current {
            if shown_geom != Some(g.geom) {
                print_geometry(&g.geom);
                if let Err(e) = check_fits(&tpl, &g.geom) {
                    result = Err(e);
                    break;
                }
                shown_geom = Some(g.geom);
            }
            let (m, compute) = measure(&tpl, &g.geom, &g.rgba);
            total += compute;
            count += 1;
            if let Err(e) = report(&mut csv, t0.elapsed().as_secs_f64(), g.seq, &m, &g.geom, compute) {
                result = Err(e);
                break;
            }
        }

        next += period;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            // En retard (calcul plus long que la periode): on repart de maintenant.
            next = now;
        }
    }

    if count > 0 {
        println!(
            "Temps de calcul moyen : {:.3} ms/frame sur {count} mesures (gris + NCC)",
            total.as_secs_f64() * 1000.0 / f64::from(count)
        );
    } else {
        println!("Aucune frame recue.");
    }

    if control.is_finished() {
        control.wait().map_err(|e| format!("la capture s'est arretee: {e}"))?;
    } else {
        control.stop().map_err(|e| format!("arret de la capture: {e}"))?;
    }
    result
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
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Snap { region, out, delay } => {
            region.validate()?;
            cmd_snap(region, out, *delay)
        }
        Cmd::Extract { region, out, from, delay } => {
            region.validate()?;
            cmd_extract(region, out, from.as_deref(), *delay)
        }
        Cmd::Watch { region, template, hz, log, from } => {
            region.validate()?;
            cmd_watch(region, template, *hz, log.as_deref(), from.as_deref())
        }
    }
}
