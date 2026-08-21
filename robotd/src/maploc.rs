//! robotd's mapping host: the `maploc` pipeline on its own worker thread.
//!
//! Off unless `[maploc] enabled = true` in robotd.toml. The design promise
//! is that the control loop pays a `try_send` per tick and nothing more:
//!
//!   - odometry, posture, head joints and the moving flag arrive from the
//!     loop as one small struct per tick over a bounded channel that drops
//!     when full — mapping lag can never become loop backpressure;
//!   - depth frames arrive on the same channel from a tokio task that
//!     subscribes to `tofd`'s socket like any other client, reconnecting
//!     with backoff — tofd down means mapping idles, not robotd caring;
//!   - the worker thread runs niced (+10): the scheduler gives the control
//!     loop the core whenever they compete.
//!
//! Frames are reprojected through the head FK with the IMU-levelled floor
//! filter, and — in stop-and-scan mode — vetted by the still-window
//! accumulator before anything inks the map (see `maploc::accumulator` for
//! why). Posture pairing uses the newest odometry tick: over a unix socket
//! the frame is milliseconds old, and stop-and-scan integrates only while
//! standing, where the pose is not going anywhere.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use duck_ipc_proto as proto;
use kinematics::tof::{Posture, Reprojector};
use maploc::accumulator::{AccumulatorConfig, WindowAccumulator};
use maploc::pipeline::{Slam, SlamConfig};
use maploc::session::SessionState;
use maploc::submap::Scan;

use crate::params::{MaplocMode, MaplocParams};

/// The loop-side channel depth. Sized for ~2 s of ticks plus frames: if the
/// worker stalls longer than that, dropping samples is the correct behaviour
/// (odometry deltas re-fold on the next accepted sample; a dropped depth
/// frame is one of fifteen a second).
const EVENT_BUFFER: usize = 128;

/// ST's status codes for a usable range — the same wire contract
/// `robotctl`'s monitor applies, restated here because robotd deliberately
/// never links the `tof` driver crate.
const TOF_STATUS_VALID: [u8; 2] = [5, 9];

/// Autosave cadence. Sessions are small (a few hundred KB) and the write is
/// atomic, but flash on the board is not free — once a minute is plenty for
/// a map that took minutes to walk.
const AUTOSAVE_EVERY: Duration = Duration::from_secs(60);

/// Map publish cadence when someone is subscribed.
const PUBLISH_EVERY: Duration = Duration::from_secs(1);

/// A still window is flushed into the map after this long even if the robot
/// keeps standing — the first field test stood the robot in place, watched
/// the map, and saw nothing: the window only integrated on the next MOTION,
/// which never came. The map must build while you look at it.
const WINDOW_FLUSH_AFTER: Duration = Duration::from_secs(3);

/// One tick's worth of the robot's own state, as the control loop sees it.
#[derive(Debug, Clone, Copy)]
pub struct OdomSample {
    /// Contact odometry x, y, yaw.
    pub odom: (f32, f32, f32),
    /// Projected gravity in the trunk frame.
    pub gravity: [f64; 3],
    /// Odometry's trunk height above the floor, metres.
    pub trunk_z: f64,
    /// `[neck_pitch, head_pitch, head_yaw, head_roll]`, measured.
    pub head: [f64; 4],
    /// The loop's own "the robot is doing something" verdict.
    pub moving: bool,
}

enum Event {
    Odom(OdomSample),
    Frame(Box<proto::TofFrame>),
}

/// Handle the rest of robotd holds. Dropping every clone (robotd shutting
/// down) closes the channel; the worker saves the session and exits.
#[derive(Clone)]
pub struct Host {
    tx: mpsc::SyncSender<Event>,
}

impl Host {
    /// Feed one control-loop tick. Never blocks: a full channel drops the
    /// sample, and the next one carries the newer truth anyway.
    pub fn observe(&self, sample: OdomSample) {
        let _ = self.tx.try_send(Event::Odom(sample));
    }

    /// Feed one depth frame (called from the tofd subscription task).
    pub fn frame(&self, frame: proto::TofFrame) {
        let _ = self.tx.try_send(Event::Frame(Box::new(frame)));
    }
}

/// Start the worker. `map_tx` is where rendered maps go; the connection
/// handler hands subscribers its receivers.
pub fn spawn(
    params: MaplocParams,
    map_tx: tokio::sync::broadcast::Sender<proto::MapFrame>,
) -> Host {
    let (tx, rx) = mpsc::sync_channel(EVENT_BUFFER);
    std::thread::Builder::new()
        .name("maploc".into())
        .spawn(move || {
            // The control loop must win every contest for a core. PRIO_PROCESS
            // with tid 0 renices only this thread on Linux.
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
            }
            worker(params, rx, map_tx);
        })
        .expect("spawning the maploc thread cannot fail");
    Host { tx }
}

fn worker(
    params: MaplocParams,
    rx: mpsc::Receiver<Event>,
    map_tx: tokio::sync::broadcast::Sender<proto::MapFrame>,
) {
    let mut slam = if params.wipe_on_boot {
        tracing::info!("maploc: starting fresh (wipe_on_boot)");
        Slam::new(SlamConfig::default())
    } else {
        match SessionState::load(&params.map_path) {
            Ok(Some(session)) => {
                tracing::info!(path = %params.map_path.display(), "maploc: resumed saved session");
                Slam::from_session(SlamConfig::default(), session)
            }
            Ok(None) => Slam::new(SlamConfig::default()),
            Err(e) => {
                tracing::warn!(error = %e, "maploc: saved session unreadable; starting fresh");
                Slam::new(SlamConfig::default())
            }
        }
    };

    let reprojector = Reprojector::alpha();
    let mut acc = WindowAccumulator::new(AccumulatorConfig::default());
    let started = Instant::now();
    let mut latest: Option<OdomSample> = None;
    let mut was_still = false;
    // Stillness from odometry itself, not just the commanded flag: a robot
    // pushed by hand is moving whatever the loop thinks it asked for.
    let mut odom_window: Vec<(Instant, f32, f32, f32)> = Vec::new();
    let mut last_publish = Instant::now();
    let mut last_save = Instant::now();
    let mut seq = 0u64;
    let mut unsaved = false;
    let mut windows = 0u32;
    let mut window_opened: Option<Instant> = None;

    // Blocking recv; a closed channel is the shutdown signal.
    while let Ok(event) = rx.recv() {
        match event {
            Event::Odom(sample) => {
                let now = Instant::now();
                slam.observe_odom(sample.odom);
                odom_window.push((now, sample.odom.0, sample.odom.1, sample.odom.2));
                odom_window
                    .retain(|(at, ..)| now.duration_since(*at) <= Duration::from_millis(500));

                let still = !sample.moving
                    && odom_window.first().is_some_and(|first| {
                        let dx = sample.odom.0 - first.1;
                        let dy = sample.odom.1 - first.2;
                        let dyaw = wrap_pi(sample.odom.2 - first.3);
                        (dx * dx + dy * dy).sqrt() < 0.01 && dyaw.abs() < 0.05
                    });

                // A window flushes when the stand ends — or after
                // WINDOW_FLUSH_AFTER while it continues, so the map builds
                // while you watch instead of waiting for the next step.
                let stand_ended = was_still && !still;
                let ripe = window_opened.is_some_and(|at| at.elapsed() >= WINDOW_FLUSH_AFTER);
                if (stand_ended || ripe)
                    && !acc.is_empty()
                    && let Some((pose, composite)) = acc.finish()
                {
                    tracing::info!(
                        beams = composite.n_valid(),
                        windows = windows + 1,
                        "maploc: still window integrated"
                    );
                    slam.integrate(pose, &composite);
                    windows += 1;
                    unsaved = true;
                    window_opened = None;
                }
                if stand_ended {
                    window_opened = None;
                }
                was_still = still;
                latest = Some(sample);

                if slam.tick(started.elapsed().as_secs_f32()) {
                    unsaved = true;
                }
            }
            Event::Frame(frame) => {
                let Some(sample) = latest else { continue };
                let Some(ranges) = decode_ranges(&frame) else {
                    continue;
                };
                let posture = Posture {
                    gravity: sample.gravity,
                    trunk_height_m: (sample.trunk_z > 0.02).then_some(sample.trunk_z),
                };
                let flat = reprojector.flatten(&ranges, sample.head, &posture);
                if flat.angles_body.is_empty() {
                    continue;
                }
                let scan = Scan::from_polar(&flat.angles_body, &flat.ranges, flat.sensor_xy, 1e-3);
                match params.mode {
                    MaplocMode::StopAndScan => {
                        if was_still {
                            if acc.is_empty() {
                                window_opened = Some(Instant::now());
                            }
                            acc.push(slam.tracked(), scan);
                        }
                    }
                    MaplocMode::Continuous => {
                        slam.integrate(slam.tracked(), &scan);
                        unsaved = true;
                    }
                }
            }
        }

        if map_tx.receiver_count() > 0 && last_publish.elapsed() >= PUBLISH_EVERY {
            last_publish = Instant::now();
            if let Some(frame) = render_frame(&slam, &mut seq, windows, was_still) {
                let _ = map_tx.send(frame);
            }
        }

        if unsaved && last_save.elapsed() >= AUTOSAVE_EVERY {
            last_save = Instant::now();
            match slam.save(&params.map_path) {
                Ok(()) => unsaved = false,
                Err(e) => tracing::warn!(error = %e, "maploc: autosave failed"),
            }
        }
    }

    // Shutdown: the session is the product; losing the last minute of a
    // mapping walk to a restart would be a sour note to end on.
    if unsaved {
        if let Err(e) = slam.save(&params.map_path) {
            tracing::warn!(error = %e, "maploc: final save failed");
        } else {
            tracing::info!(path = %params.map_path.display(), "maploc: session saved");
        }
    }
}

/// The wire frame's 64 zones as metres, `None` where the sensor said the
/// measurement is not to be trusted.
fn decode_ranges(
    frame: &proto::TofFrame,
) -> Option<[Option<f64>; kinematics::tof::ROWS * kinematics::tof::COLS]> {
    const N: usize = kinematics::tof::ROWS * kinematics::tof::COLS;
    if frame.distance_mm.len() != N || frame.status.len() != N {
        return None;
    }
    let mut out = [None; N];
    for (slot, (&mm, &status)) in out
        .iter_mut()
        .zip(frame.distance_mm.iter().zip(frame.status.iter()))
    {
        if TOF_STATUS_VALID.contains(&status) && mm > 0 {
            *slot = Some(f64::from(mm) / 1000.0);
        }
    }
    Some(out)
}

/// The composite map as a wire frame: trinary cells, base64.
fn render_frame(slam: &Slam, seq: &mut u64, windows: u32, still: bool) -> Option<proto::MapFrame> {
    let grid = slam.render()?;
    let mut cells = Vec::with_capacity(grid.width() * grid.height());
    for i in 0..grid.height() {
        for j in 0..grid.width() {
            let lo = grid.log_at(i, j);
            cells.push(if lo > 150 {
                2u8
            } else if lo < -50 {
                1
            } else {
                0
            });
        }
    }
    let (x, y, yaw) = slam.tracked();
    *seq += 1;
    Some(proto::MapFrame {
        seq: *seq,
        x: f64::from(x),
        y: f64::from(y),
        yaw: f64::from(yaw),
        tracking: slam.n_submaps() > 0,
        x_min: grid.cfg().x_range.0,
        y_min: grid.cfg().y_range.0,
        cell_m: grid.cell(),
        rows: grid.height() as u32,
        cols: grid.width() as u32,
        cells: proto::b64::encode(&cells),
        n_submaps: slam.n_submaps() as u32,
        n_loops: slam.n_loops() as u32,
        windows,
        still,
    })
}

/// Subscribe to `tofd`'s depth stream and pump frames into the host.
/// Reconnects with backoff forever: tofd restarting (or absent on a board
/// with no sensor) idles mapping, nothing more.
pub async fn feed_tof(host: Host) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut backoff = Duration::from_millis(500);
    loop {
        match tokio::net::UnixStream::connect(proto::socket::TOF).await {
            Ok(stream) => {
                backoff = Duration::from_millis(500);
                let (read, mut write) = stream.into_split();
                let hello = serde_json::to_string(&proto::Request::call(
                    proto::Id::Number(1),
                    &proto::Call::Hello(proto::HelloParams {
                        api_version: proto::API_VERSION,
                    }),
                ))
                .expect("hello serializes");
                let subscribe = serde_json::to_string(&proto::Request::call(
                    proto::Id::Number(2),
                    &proto::Call::TofStream,
                ))
                .expect("subscribe serializes");
                if write
                    .write_all(format!("{hello}\n{subscribe}\n").as_bytes())
                    .await
                    .is_err()
                {
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(request) = serde_json::from_str::<proto::Request>(&line)
                        && let Some(frame) = request.as_tof_frame()
                    {
                        host.frame(frame);
                    }
                }
                tracing::debug!("maploc: tofd stream ended; reconnecting");
            }
            Err(_) => {
                // Absent socket is the no-sensor board; stay quiet about it.
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
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
