//! The mapping host loop — stillness, windows, the tracking watchdog and
//! kidnap recovery — shared by robotd's worker and the offline bench.
//!
//! [`crate::pipeline::Slam`] exists so the graph wiring is written once;
//! this module exists for the same reason one level up. The still
//! detector, the window flush rules, the quality gates and the
//! lost/relocalize state machine used to live in robotd's worker, with the
//! replay bench keeping a hand-mirrored copy — the exact arrangement whose
//! drift the pipeline module was created to prevent. A ground-truth
//! recording is only worth its disk space if the bench replays it through
//! the *same* decisions the robot made, so the decisions moved here and
//! both hosts drive this.
//!
//! The state machine, in one paragraph: scans integrate only through
//! vetted still windows (see [`crate::accumulator`]); before a window
//! inks, it is scored against the map at the tracked pose
//! ([`crate::relocalize::score_pose`]) and a window the map can judge but
//! flatly contradicts flips the mapper to *lost* — a kidnapped robot's
//! scans land in territory the map knows and disagree everywhere, while a
//! robot exploring a new room lands in territory the map cannot judge and
//! keeps mapping. While lost, nothing inks and every window becomes a
//! brute-force relocalize attempt ([`crate::relocalize`]); an accepted
//! pose snaps tracking there and mapping resumes. The same watchdog heals
//! a resumed session whose robot moved while the daemon was down.

use crate::accumulator::{AccumulatorConfig, WindowAccumulator};
use crate::pipeline::Slam;
use crate::relocalize::{RelocalizeConfig, relocalize_against_grid, score_pose};
use crate::submap::{Pose2, Scan};

/// Stillness from odometry itself, not just the host's moving flag: a
/// robot pushed by hand is moving whatever the control loop asked for.
#[derive(Debug, Clone, Copy)]
pub struct StillConfig {
    /// Displacement window length.
    pub window_s: f32,
    /// Max translation across the window to count as still.
    pub max_dxy_m: f32,
    /// Max |yaw| across the window to count as still.
    pub max_dyaw_rad: f32,
}

impl Default for StillConfig {
    fn default() -> Self {
        Self {
            window_s: 0.5,
            max_dxy_m: 0.01,
            max_dyaw_rad: 0.05,
        }
    }
}

/// The tracking watchdog's thresholds. All three must hold to declare
/// tracking lost — the bar is deliberately high, because a false "lost"
/// stops the map cold until a relocalize succeeds.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogConfig {
    /// The map must be able to judge at least this many beams.
    pub min_observed_beams: u32,
    /// ... and at least this fraction of the window's beams.
    pub min_observed_fraction: f32,
    /// Mean residual over the judged beams above which the window is a
    /// contradiction, not noise. Map noise floor is ~0.05–0.09 m; honest
    /// inter-stop drift stays well under 0.2 m.
    pub max_mean_residual_m: f32,
    /// Per-beam residual clamp for the score.
    pub clamp_m: f32,
    /// A cell is a wall for the distance field past this. 150 matches the
    /// wire frame's wall definition: one double-inked window (2 × 85)
    /// qualifies, so a thinly-mapped revisit is not scored against a
    /// field that pretends its own walls are not there.
    pub wall_threshold_fp: i16,
    /// A cell is *observed* (judgeable) past this |log-odds|.
    pub observed_fp: i16,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            min_observed_beams: 100,
            min_observed_fraction: 0.4,
            max_mean_residual_m: 0.25,
            clamp_m: 0.5,
            wall_threshold_fp: 150,
            observed_fp: 50,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MapperConfig {
    /// `false` = stop-and-scan (windows, votes, gates); `true` = ink every
    /// frame directly (more coverage, blurrier walls, no watchdog).
    pub continuous: bool,
    pub accumulator: AccumulatorConfig,
    pub still: StillConfig,
    /// A still window flushes after this long even if the stand continues,
    /// so the map builds while you watch it.
    pub window_flush_after_s: f32,
    /// A vetted window with fewer beams is discarded, not inked — a seated
    /// robot's floor-clutter windows measured 2–27 beams; a real stop
    /// measures in the hundreds.
    pub min_window_beams: usize,
    /// How many times a vetted window inks. One pass writes log-odds 85
    /// per wall cell and a wall starts at 150, so a lap that stops once
    /// per spot would paint itself invisibly; a window has survived
    /// per-cell frame voting and is worth more than one raw frame.
    pub window_ink_passes: usize,
    pub watchdog: WatchdogConfig,
    pub relocalize: RelocalizeConfig,
    /// A relocalize probe is the composite decimated to at most this many
    /// beams: a window composite carries thousands, and the brute-force
    /// search is O(cells × yaws × beams) — full composites would cost
    /// seconds per attempt on the robot for no accuracy the search needs.
    pub relocalize_max_beams: usize,
}

impl Default for MapperConfig {
    fn default() -> Self {
        Self {
            continuous: false,
            accumulator: AccumulatorConfig::default(),
            still: StillConfig::default(),
            window_flush_after_s: 3.0,
            min_window_beams: 60,
            window_ink_passes: 2,
            watchdog: WatchdogConfig::default(),
            relocalize: RelocalizeConfig {
                // Align the search's idea of a wall with the watchdog's
                // (and the 2×-ink reality) — the stock 200 was tuned on
                // prototype captures inked far more than twice.
                wall_threshold_fp: 150,
                // A composite worth relocalizing on carries ≥ 60 beams
                // (min_window_beams); demanding that many *in the map*
                // kills the measured failure mode where a 44-beam wedge
                // "accepted" a pose across the room.
                min_beams_used: 60,
                ..RelocalizeConfig::default()
            },
            relocalize_max_beams: 256,
        }
    }
}

/// One control-loop tick's worth of the robot, as the mapper needs it.
/// (Gravity, trunk height and head joints feed the *reprojection*, which
/// stays in the host — this crate never links the kinematics.)
#[derive(Debug, Clone, Copy)]
pub struct MapperSample {
    pub odom: Pose2,
    /// The host's "the robot is doing something" verdict.
    pub moving: bool,
    /// Seated: never map from sitting height — the ToF sees knees and
    /// floor clutter, and the ground-truth protocol uses the sit as its
    /// kidnap marker.
    pub sitting: bool,
}

/// What one call did — the host turns these into log lines; the bench
/// turns them into metrics. Data, not strings, so both can.
#[derive(Debug, Clone, Copy)]
pub enum Note {
    WindowIntegrated {
        beams: usize,
        windows: u32,
    },
    WindowDiscarded {
        beams: usize,
    },
    /// The map could judge this window and flatly contradicts it.
    LostTracking {
        mean_residual_m: f32,
        n_observed: u32,
    },
    Relocalized {
        pose: Pose2,
        mean_residual_m: f32,
    },
    RelocalizeRejected {
        best_pose: Pose2,
        mean_residual_m: f32,
    },
    LoopClosed {
        n_loops: usize,
        dx: f32,
        dy: f32,
        dyaw: f32,
    },
}

pub struct Mapper {
    cfg: MapperConfig,
    slam: Slam,
    acc: WindowAccumulator,
    /// (t_s, x, y, yaw) over the last `still.window_s`.
    odom_window: Vec<(f32, f32, f32, f32)>,
    was_still: bool,
    window_opened: Option<f32>,
    windows: u32,
    lost: bool,
}

impl Mapper {
    pub fn new(cfg: MapperConfig, slam: Slam) -> Self {
        Self {
            acc: WindowAccumulator::new(cfg.accumulator),
            cfg,
            slam,
            odom_window: Vec::new(),
            was_still: false,
            window_opened: None,
            windows: 0,
            lost: false,
        }
    }

    pub fn slam(&self) -> &Slam {
        &self.slam
    }
    pub fn slam_mut(&mut self) -> &mut Slam {
        &mut self.slam
    }
    pub fn windows(&self) -> u32 {
        self.windows
    }
    pub fn still(&self) -> bool {
        self.was_still
    }
    /// False while lost (kidnapped, or a resumed session the scans refute).
    pub fn tracking(&self) -> bool {
        !self.lost
    }
    /// Frames sitting in the open still window.
    pub fn window_frames(&self) -> usize {
        self.acc.len()
    }

    /// One control-loop tick. `t_s` is seconds on any monotonic timebase —
    /// the host's uptime, a recording's timestamps — as long as one mapper
    /// sees only one. Notes are appended, not replaced.
    pub fn observe(&mut self, t_s: f32, sample: MapperSample, notes: &mut Vec<Note>) {
        self.slam.observe_odom(sample.odom);
        self.odom_window
            .push((t_s, sample.odom.0, sample.odom.1, sample.odom.2));
        let horizon = self.cfg.still.window_s;
        self.odom_window.retain(|&(at, ..)| t_s - at <= horizon);

        let still = !sample.moving
            && !sample.sitting
            && self.odom_window.first().is_some_and(|&(_, fx, fy, fyaw)| {
                let dx = sample.odom.0 - fx;
                let dy = sample.odom.1 - fy;
                let dyaw = wrap_pi(sample.odom.2 - fyaw);
                (dx * dx + dy * dy).sqrt() < self.cfg.still.max_dxy_m
                    && dyaw.abs() < self.cfg.still.max_dyaw_rad
            });

        // A window flushes when the stand ends — or after
        // `window_flush_after_s` while it continues, so the map builds
        // while you watch instead of waiting for the next step. The window
        // closes here whatever comes of it: leaving it armed after a
        // fruitless finish would flush every subsequent frame alone.
        let stand_ended = self.was_still && !still;
        let ripe = self
            .window_opened
            .is_some_and(|t0| t_s - t0 >= self.cfg.window_flush_after_s);
        if (stand_ended || ripe) && !self.acc.is_empty() {
            self.window_opened = None;
            if let Some((pose, composite)) = self.acc.finish() {
                self.absorb_window(pose, &composite, notes);
            }
        }
        if stand_ended {
            self.window_opened = None;
        }
        self.was_still = still;

        // While lost the tracked pose is a guess; freezing submaps or
        // running closures on it would launder the guess into the graph.
        if !self.lost {
            let loops_before = self.slam.n_loops();
            let before = self.slam.tracked();
            self.slam.tick(t_s);
            if self.slam.n_loops() > loops_before {
                let after = self.slam.tracked();
                notes.push(Note::LoopClosed {
                    n_loops: self.slam.n_loops(),
                    dx: after.0 - before.0,
                    dy: after.1 - before.1,
                    dyaw: wrap_pi(after.2 - before.2),
                });
            }
        }
    }

    /// One reprojected depth frame, already in the body frame. Returns
    /// true when the frame was kept (accumulated or inked).
    pub fn frame(&mut self, t_s: f32, scan: Scan) -> bool {
        if self.cfg.continuous {
            if !self.lost {
                self.slam.integrate(self.slam.tracked(), &scan);
            }
            return !self.lost;
        }
        if !self.was_still {
            return false;
        }
        if self.acc.is_empty() {
            self.window_opened = Some(t_s);
        }
        self.acc.push(self.slam.tracked(), scan);
        true
    }

    fn absorb_window(&mut self, pose: Pose2, composite: &Scan, notes: &mut Vec<Note>) {
        let beams = composite.n_valid();
        if beams < self.cfg.min_window_beams {
            notes.push(Note::WindowDiscarded { beams });
            return;
        }

        if self.lost {
            let Some(mut grid) = self.slam.render() else {
                return;
            };
            let probe = decimate(composite, self.cfg.relocalize_max_beams);
            match relocalize_against_grid(&mut grid, &probe, &self.cfg.relocalize) {
                Some(r) if r.accepted => {
                    self.slam.set_tracked(r.pose);
                    self.ink(r.pose, composite);
                    self.lost = false;
                    notes.push(Note::Relocalized {
                        pose: r.pose,
                        mean_residual_m: r.mean_residual_m,
                    });
                }
                Some(r) => notes.push(Note::RelocalizeRejected {
                    best_pose: r.pose,
                    mean_residual_m: r.mean_residual_m,
                }),
                None => {}
            }
            return;
        }

        // The watchdog: score the window against the map before believing
        // it. A window the map can judge but contradicts must not ink — it
        // would paint the kidnapper's room over the real one.
        let wd = self.cfg.watchdog;
        if let Some(mut grid) = self.slam.render() {
            let a = score_pose(
                &mut grid,
                composite,
                pose,
                wd.clamp_m,
                wd.wall_threshold_fp,
                wd.observed_fp,
            );
            if a.n_observed >= wd.min_observed_beams
                && a.n_observed as f32 >= wd.min_observed_fraction * a.n_beams as f32
                && a.mean_residual_m > wd.max_mean_residual_m
            {
                self.lost = true;
                notes.push(Note::LostTracking {
                    mean_residual_m: a.mean_residual_m,
                    n_observed: a.n_observed,
                });
                return;
            }
        }
        self.ink(pose, composite);
        notes.push(Note::WindowIntegrated {
            beams,
            windows: self.windows,
        });
    }

    fn ink(&mut self, pose: Pose2, composite: &Scan) {
        for _ in 0..self.cfg.window_ink_passes {
            self.slam.integrate(pose, composite);
        }
        self.windows += 1;
    }
}

/// Every k-th beam, sized to land at or under `max_beams`.
fn decimate(scan: &Scan, max_beams: usize) -> Scan {
    let n = scan.beams.len();
    if n <= max_beams.max(1) {
        return scan.clone();
    }
    let step = n.div_ceil(max_beams.max(1));
    Scan {
        beams: scan.beams.iter().copied().step_by(step).collect(),
    }
}

fn wrap_pi(a: f32) -> f32 {
    use std::f32::consts::PI;
    let two_pi = 2.0 * PI;
    let mut y = (a + PI).rem_euclid(two_pi) - PI;
    if y == PI {
        y = -PI;
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{Slam, SlamConfig};

    /// What the sensor sees standing at `pose` (the robot's TRUE pose) in
    /// a rectangular room (walls at x = ±1.5, y = ±1.1): body-frame
    /// angles, true ranges. Rectangular on purpose — a square room is
    /// 90°-symmetric and a relocalizer handed one is *entitled* to pick a
    /// rotated pose.
    /// Where the mapper paints them is its own business — that gap is what
    /// the kidnap test exercises.
    fn room_scan(pose: Pose2) -> Scan {
        let mut angles = Vec::new();
        let mut ranges = Vec::new();
        for k in 0..240 {
            let a = -std::f32::consts::PI + k as f32 * (2.0 * std::f32::consts::PI / 240.0);
            let (dx, dy) = ((pose.2 + a).cos(), (pose.2 + a).sin());
            let tx = if dx > 1e-6 {
                (1.5 - pose.0) / dx
            } else if dx < -1e-6 {
                (-1.5 - pose.0) / dx
            } else {
                f32::INFINITY
            };
            let ty = if dy > 1e-6 {
                (1.1 - pose.1) / dy
            } else if dy < -1e-6 {
                (-1.1 - pose.1) / dy
            } else {
                f32::INFINITY
            };
            let mut r = tx.min(ty);
            // A half-divider at x = 0.3, y ∈ [-1.1, 0] breaks the
            // rectangle's remaining 180° symmetry.
            if dx.abs() > 1e-6 {
                let td = (0.3 - pose.0) / dx;
                if td > 0.0 {
                    let y_hit = pose.1 + td * dy;
                    if (-1.1..=0.0).contains(&y_hit) {
                        r = r.min(td);
                    }
                }
            }
            if r.is_finite() && r < 1.9 {
                angles.push(a);
                ranges.push(r);
            }
        }
        Scan::from_polar(&angles, &ranges, (0.0, 0.0), 1e-3)
    }

    fn drive(
        mapper: &mut Mapper,
        t0: f32,
        pose: Pose2,
        seconds: f32,
        notes: &mut Vec<Note>,
    ) -> f32 {
        // 50 Hz odometry, 15 Hz frames, robot standing at `pose`.
        let mut t = t0;
        let end = t0 + seconds;
        let mut next_frame = t0;
        while t < end {
            mapper.observe(
                t,
                MapperSample {
                    odom: pose,
                    moving: false,
                    sitting: false,
                },
                notes,
            );
            if t >= next_frame {
                mapper.frame(t, room_scan(pose));
                next_frame += 1.0 / 15.0;
            }
            t += 0.02;
        }
        t
    }

    /// The whole point of the machine: a kidnap (scans that contradict the
    /// map) flips tracking to lost, nothing inks, and a good window
    /// relocalizes back to the true pose.
    #[test]
    fn a_kidnapped_mapper_stops_relocates_and_resumes() {
        let mut mapper = Mapper::new(MapperConfig::default(), Slam::new(SlamConfig::default()));
        let mut notes = Vec::new();

        // Build a map from two stands — a second viewpoint fills the
        // divider's occlusion shadow, exactly like a real mapping lap
        // does; a single-viewpoint map penalizes the true post-kidnap
        // pose for beams into the territory only the kidnapper's side of
        // the room can see.
        let mut t = drive(&mut mapper, 0.0, (0.0, 0.0, 0.0), 8.0, &mut notes);
        t = drive(&mut mapper, t, (0.9, -0.6, -1.2), 8.0, &mut notes);
        assert!(mapper.windows() >= 2, "the stands must have inked windows");
        assert!(mapper.tracking());

        // Kidnap: odometry still reads the origin, but the robot now really
        // stands at (0.8, 0.5, 0.9) — its scans are the room seen from
        // there, expressed in the body frame odometry believes in.
        let truth = (0.8, 0.5, 0.9);
        let mut next_frame = t;
        let end = t + 20.0;
        let mut relocalized = None;
        while t < end {
            mapper.observe(
                t,
                MapperSample {
                    odom: (0.0, 0.0, 0.0),
                    moving: false,
                    sitting: false,
                },
                &mut notes,
            );
            if t >= next_frame {
                mapper.frame(t, room_scan(truth));
                next_frame += 1.0 / 15.0;
            }
            for note in notes.drain(..) {
                if let Note::Relocalized { pose, .. } = note {
                    relocalized = Some(pose);
                }
            }
            if relocalized.is_some() {
                break;
            }
            t += 0.02;
        }

        let pose = relocalized.expect("the mapper must relocalize after a kidnap");
        let err = (pose.0 - truth.0).hypot(pose.1 - truth.1);
        assert!(err < 0.25, "relocalized {pose:?}, truth {truth:?}");
        assert!(wrap_pi(pose.2 - truth.2).abs() < 0.3);
        assert!(mapper.tracking());
    }

    /// Beams into unexplored territory must never read as "lost".
    #[test]
    fn exploring_new_territory_is_not_a_kidnap() {
        let mut mapper = Mapper::new(MapperConfig::default(), Slam::new(SlamConfig::default()));
        let mut notes = Vec::new();
        let t = drive(&mut mapper, 0.0, (0.0, 0.0, 0.0), 8.0, &mut notes);
        // Face the other way from a spot the map has never judged: the
        // scans land in unknown cells.
        drive(&mut mapper, t, (0.4, -0.3, 2.5), 8.0, &mut notes);
        assert!(
            mapper.tracking(),
            "new territory must be mapped, not declared a kidnap"
        );
    }
}
