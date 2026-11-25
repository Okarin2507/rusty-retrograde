use std::time::Instant;

use iced::widget::canvas::{self, Path};
use iced::{Point, Size};

const OPEN_MS: f32 = 700.0;
const CLOSE_MS: f32 = 420.0;
const BLOT_POINTS: usize = 18;
pub(super) const COVER_SHARE: f32 = 0.95;
const BLOT_REACH: f32 = 1.12;

#[derive(Debug, Clone, Copy)]
pub struct Droplet {
    pub angle: f32,
    pub distance: f32,
    pub size: f32,
    pub phase: f32,
}

/// The reveal in progress: how far it had come when it started and which way it runs.
#[derive(Debug, Clone, Copy)]
pub struct FsAnim {
    pub run_start: Instant,
    pub leg_start: Instant,
    pub opening: bool,
    pub from: f32,
    pub phases: [f32; 6],
    pub drops: [Droplet; 4],
}

pub(super) fn seeded_blot_params(to_centre: f32) -> ([f32; 6], [Droplet; 4]) {
    let mut s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
        | 1;
    let mut next_01 = || -> f32 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 40) as f32) / ((1u32 << 24) as f32)
    };
    let mut phases = [0.0f32; 6];
    for p in &mut phases {
        *p = next_01() * std::f32::consts::TAU;
    }
    let mut drops = [Droplet {
        angle: 0.0,
        distance: 0.0,
        size: 0.0,
        phase: 0.0,
    }; 4];
    for (i, slot) in drops.iter_mut().enumerate() {
        *slot = Droplet {
            angle: to_centre + (next_01() - 0.5) * 1.6,
            distance: 0.22 + 0.16 * (i as f32) + next_01() * 0.08,
            size: 0.035 + next_01() * 0.03,
            phase: next_01() * std::f32::consts::TAU,
        };
    }
    (phases, drops)
}

impl FsAnim {
    fn progress(&self, now: Instant) -> f32 {
        let ms = now.saturating_duration_since(self.leg_start).as_secs_f32() * 1000.0;
        let span = if self.opening {
            (1.0 - self.from) * OPEN_MS
        } else {
            self.from * CLOSE_MS
        };
        if span <= 0.0 {
            1.0
        } else {
            (ms / span).clamp(0.0, 1.0)
        }
    }

    pub fn share_at(&self, now: Instant) -> f32 {
        let t = self.progress(now);
        if self.opening {
            self.from + (1.0 - self.from) * (1.0 - (1.0 - t).powi(4))
        } else {
            self.from * (1.0 - t * t * t)
        }
    }

    pub fn finished(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }
}

fn blot_wobble(share: f32) -> f32 {
    0.16 * (1.0 - share).max(0.0).powf(1.3) + 0.025
}

pub(super) fn content_alpha(share: f32) -> f32 {
    ((share - COVER_SHARE) / (1.0 - COVER_SHARE)).clamp(0.0, 1.0)
}

/// The blot's edge: an 18-point Catmull-Rom lobe around the full-screen button with 4 leading droplets.
pub(super) struct Blot {
    pub share: f32,
    pub seconds: f32,
    pub phases: [f32; 6],
    pub drops: [Droplet; 4],
}

impl Blot {
    fn draw_lobe(
        &self,
        p: &mut canvas::path::Builder,
        c: Point,
        radius: f32,
        wobble: f32,
        phase_offset: f32,
        seed_points: (usize, usize),
    ) {
        let (seed, points) = seed_points;
        let seconds = self.seconds + phase_offset;
        let p0 = self.phases[(seed * 3) % self.phases.len()];
        let p1 = self.phases[(seed * 3 + 1) % self.phases.len()];
        let p2 = self.phases[(seed * 3 + 2) % self.phases.len()];
        let mut pts = [Point::ORIGIN; BLOT_POINTS];
        let n = points.min(BLOT_POINTS);
        for (i, slot) in pts[..n].iter_mut().enumerate() {
            let a = i as f32 * std::f32::consts::TAU / n as f32;
            let ripple = 0.55 * (3.0 * a + seconds * 2.2 + p0).sin()
                + 0.30 * (5.0 * a - seconds * 1.5 + p1).sin()
                + 0.15 * (8.0 * a + seconds * 3.1 + p2).sin();
            let r = radius * (1.0 + wobble * ripple);
            *slot = Point::new(c.x + a.cos() * r, c.y + a.sin() * r);
        }
        p.move_to(pts[0]);
        for i in 0..n {
            let a_pt = pts[(i + n - 1) % n];
            let b = pts[i];
            let cc = pts[(i + 1) % n];
            let d = pts[(i + 2) % n];
            let c1 = Point::new(b.x + (cc.x - a_pt.x) / 6.0, b.y + (cc.y - a_pt.y) / 6.0);
            let c2 = Point::new(cc.x - (d.x - b.x) / 6.0, cc.y - (d.y - b.y) / 6.0);
            p.bezier_curve_to(c1, c2, cc);
        }
        p.close();
    }

    pub(super) fn path(&self, size: Size) -> Option<Path> {
        if self.share <= 0.001 {
            return None;
        }
        let origin = Point::new(size.width - 36.0, size.height - 34.0);
        let far = origin
            .x
            .max(size.width - origin.x)
            .hypot(origin.y.max(size.height - origin.y));
        let radius = (far * BLOT_REACH * self.share).max(1.0);
        let wobble = blot_wobble(self.share);
        Some(Path::new(|p| {
            self.draw_lobe(p, origin, radius, wobble, 0.0, (0, BLOT_POINTS));
            for d in self.drops {
                let dist = d.distance * far;
                if dist <= 0.0 {
                    continue;
                }
                let lead = (radius / dist - 0.55) / 0.45;
                if lead <= 0.0 || lead >= 1.3 {
                    continue;
                }
                let grow = (lead.min(1.0) * std::f32::consts::FRAC_PI_2).sin();
                let r = d.size * far * grow;
                if r < 1.0 {
                    continue;
                }
                let center = Point::new(
                    origin.x + d.angle.cos() * dist,
                    origin.y + d.angle.sin() * dist,
                );
                self.draw_lobe(p, center, r, 0.12, d.phase, (1, 10));
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn anim(opening: bool, from: f32, leg_start: Instant) -> FsAnim {
        FsAnim {
            run_start: leg_start,
            leg_start,
            opening,
            from,
            phases: [0.0; 6],
            drops: [Droplet {
                angle: 0.0,
                distance: 0.0,
                size: 0.0,
                phase: 0.0,
            }; 4],
        }
    }

    #[test]
    fn blot_covers_the_window_before_the_player_fades_in() {
        for i in 0..=50 {
            let share = COVER_SHARE + (1.0 - COVER_SHARE) * i as f32 / 50.0;
            let narrowest = BLOT_REACH * share * (1.0 - blot_wobble(share));
            assert!(narrowest >= 1.03, "share {share}: narrowest {narrowest}");
        }
        assert_eq!(content_alpha(COVER_SHARE - 0.01), 0.0);
        assert_eq!(content_alpha(1.0), 1.0);
    }

    #[test]
    fn share_runs_both_legs_and_ends() {
        let t0 = Instant::now();
        let open = anim(true, 0.0, t0);
        assert_eq!(open.share_at(t0), 0.0);
        let mid = open.share_at(t0 + Duration::from_millis(350));
        assert!(mid > 0.5 && mid < 1.0);
        assert!(!open.finished(t0 + Duration::from_millis(600)));
        assert!(open.finished(t0 + Duration::from_millis(700)));
        assert_eq!(open.share_at(t0 + Duration::from_secs(5)), 1.0);

        let close = anim(false, 0.6, t0);
        assert!((close.share_at(t0) - 0.6).abs() < 1e-6);
        assert!(close.finished(t0 + Duration::from_millis(253)));
        assert_eq!(close.share_at(t0 + Duration::from_secs(1)), 0.0);
    }
}
