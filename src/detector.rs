//! Machine a etats vivant/mort: transforme le flux de mesures brutes en un
//! etat stable. Autonome (ni capture, ni systeme): une fonction du flux
//! d'entree vers un etat, rejouable depuis un CSV.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Alive,
    Dead,
}

/// Une mesure brute.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub timestamp_ms: u64,
    pub score: f32,
    pub foreground: bool,
    /// Transporte tel quel: aucune regle ne l'exploite pour l'instant.
    pub capture_ok: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// score > seuil => bandeau spectateur present => candidat Dead.
    pub threshold: f32,
    /// Frames consecutives candidates Dead pour basculer Alive -> Dead.
    pub frames_to_dead: u32,
    /// Frames consecutives candidates Alive pour basculer Dead -> Alive.
    pub frames_to_alive: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self { threshold: 0.45, frames_to_dead: 5, frames_to_alive: 2 }
    }
}

/// Pourquoi une frame n'a pas ete prise en compte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freeze {
    /// foreground = false.
    Background,
    /// score exactement 0.0: frame noire, pas une vraie correlation.
    ZeroScore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub state: State,
    /// L'etat vient de basculer sur cette frame.
    pub changed: bool,
    /// Some si la frame a ete ignoree (etat et compteur inchanges).
    pub frozen: Option<Freeze>,
}

pub struct Detector {
    cfg: Config,
    state: State,
    /// Frames consecutives dont le candidat differe de l'etat courant.
    streak: u32,
}

impl Detector {
    pub fn new(cfg: Config) -> Self {
        Self { cfg, state: State::Alive, streak: 0 }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn streak(&self) -> u32 {
        self.streak
    }

    pub fn push(&mut self, f: &Frame) -> Output {
        // Regles 1 et 2: la mesure est ignoree entierement, le compteur
        // d'hysteresis n'avance ni ne repart a zero.
        let frozen = if !f.foreground {
            Some(Freeze::Background)
        } else if f.score == 0.0 {
            Some(Freeze::ZeroScore)
        } else {
            None
        };
        if frozen.is_some() {
            return Output { state: self.state, changed: false, frozen };
        }

        let candidate = if f.score > self.cfg.threshold { State::Dead } else { State::Alive };
        let mut changed = false;
        if candidate == self.state {
            self.streak = 0;
        } else {
            self.streak += 1;
            let needed = match candidate {
                State::Dead => self.cfg.frames_to_dead,
                State::Alive => self.cfg.frames_to_alive,
            };
            if self.streak >= needed {
                self.state = candidate;
                self.streak = 0;
                changed = true;
            }
        }
        Output { state: self.state, changed, frozen: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames a 10 Hz, au premier plan, a partir de scores.
    fn frames(scores: &[f32]) -> Vec<Frame> {
        scores
            .iter()
            .enumerate()
            .map(|(i, &score)| Frame {
                timestamp_ms: i as u64 * 100,
                score,
                foreground: true,
                capture_ok: true,
            })
            .collect()
    }

    /// Rejoue et renvoie les index des frames qui ont bascule, et l'etat final.
    fn run(d: &mut Detector, fs: &[Frame]) -> (Vec<usize>, State) {
        let changes = fs.iter().enumerate().filter(|(_, f)| d.push(f).changed).map(|(i, _)| i).collect();
        (changes, d.state())
    }

    const LOW: f32 = 0.05;
    const HIGH: f32 = 0.99;

    #[test]
    fn initial_state_is_alive() {
        assert_eq!(Detector::new(Config::default()).state(), State::Alive);
    }

    #[test]
    fn single_frame_spike_while_alive_does_not_switch() {
        let mut d = Detector::new(Config::default());
        let (changes, state) = run(&mut d, &frames(&[LOW, LOW, HIGH, LOW, LOW]));
        assert!(changes.is_empty());
        assert_eq!(state, State::Alive);
    }

    #[test]
    fn single_frame_drop_while_dead_does_not_switch() {
        // Le cas des changements de joueur observe en spectateur.
        let mut d = Detector::new(Config::default());
        let mut s = vec![HIGH; 10];
        s.extend([LOW]);
        s.extend([HIGH; 10]);
        let (changes, state) = run(&mut d, &frames(&s));
        assert_eq!(changes, vec![4]);
        assert_eq!(state, State::Dead);
    }

    #[test]
    fn long_transition_switches_once_after_frames_to_dead() {
        let mut d = Detector::new(Config::default());
        let mut s = vec![LOW; 20];
        s.extend([HIGH; 50]); // 5 s de bandeau
        let (changes, state) = run(&mut d, &frames(&s));
        // 5e frame consecutive au-dessus du seuil.
        assert_eq!(changes, vec![24]);
        assert_eq!(state, State::Dead);
    }

    #[test]
    fn death_then_respawn_switches_twice_with_asymmetric_delays() {
        let mut d = Detector::new(Config::default());
        let mut s = vec![LOW; 10];
        s.extend([HIGH; 30]);
        s.extend([LOW; 30]);
        let (changes, state) = run(&mut d, &frames(&s));
        assert_eq!(changes, vec![14, 41]);
        assert_eq!(state, State::Alive);
    }

    #[test]
    fn interrupted_streak_restarts_from_zero() {
        let mut d = Detector::new(Config::default());
        let (changes, _) = run(&mut d, &frames(&[HIGH, HIGH, HIGH, HIGH, LOW, HIGH, HIGH, HIGH, HIGH]));
        assert!(changes.is_empty());
    }

    #[test]
    fn score_equal_to_threshold_is_alive() {
        let mut d = Detector::new(Config::default());
        let (changes, _) = run(&mut d, &frames(&[0.45; 20]));
        assert!(changes.is_empty());
    }

    #[test]
    fn background_freezes_even_with_aberrant_scores() {
        for start in [State::Alive, State::Dead] {
            let mut d = Detector::new(Config::default());
            if start == State::Dead {
                run(&mut d, &frames(&[HIGH; 5]));
            }
            let aberrant = if start == State::Alive { HIGH } else { -1.0 };
            let mut fs = frames(&[aberrant; 100]);
            fs.iter_mut().for_each(|f| f.foreground = false);
            for f in &fs {
                let out = d.push(f);
                assert!(!out.changed);
                assert_eq!(out.frozen, Some(Freeze::Background));
            }
            assert_eq!(d.state(), start);
            assert_eq!(d.streak(), 0);
        }
    }

    #[test]
    fn background_frames_neither_advance_nor_reset_the_streak() {
        let mut d = Detector::new(Config::default());
        let mut fs = frames(&[HIGH; 9]);
        for f in &mut fs[2..6] {
            f.foreground = false;
        }
        let (changes, _) = run(&mut d, &fs);
        // 2 frames avant le gel + 3 apres: la 5e prise en compte est l'index 8.
        assert_eq!(changes, vec![8]);
    }

    #[test]
    fn zero_scores_freeze_the_state() {
        // Dead puis ecran noir: sans la regle, 0.0 <= seuil ferait revivre.
        let mut d = Detector::new(Config::default());
        let mut s = vec![HIGH; 10];
        s.extend([0.0; 50]);
        let fs = frames(&s);
        let (changes, state) = run(&mut d, &fs);
        assert_eq!(changes, vec![4]);
        assert_eq!(state, State::Dead);
        let out = d.push(&fs[20]);
        assert_eq!(out.frozen, Some(Freeze::ZeroScore));
    }

    #[test]
    fn parameters_are_taken_from_config() {
        let cfg = Config { threshold: 0.8, frames_to_dead: 1, frames_to_alive: 3 };
        let mut d = Detector::new(cfg);
        let (changes, _) = run(&mut d, &frames(&[0.7, 0.9, 0.1, 0.1, 0.1]));
        assert_eq!(changes, vec![1, 4]);
    }
}
