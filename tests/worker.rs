//! The microphone worker's protocol, pipes and job object, with simulated workers:
//! `platinum mic-worker --simulate …` opens no audio device, loads nothing of the vendor's and
//! writes nothing. Ignored by default, since it starts processes:
//! `cargo test --release --test worker -- --ignored`.

use platinum::worker::{Args, Command, EXIT_GRACE, Event, Job, Report, Simulation, Worker};
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(5);

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_platinum"))
}

fn simulated(simulation: Simulation, hold: bool) -> Args {
    Args { source: String::new(), target: String::new(), effects_dll: Some("simulated.dll".into()), gain: 1.0, latency_ms: 30, hold, simulate: Some(simulation) }
}

/// The first report from `pid` that `wanted` accepts, within `limit`.
fn wait_for(events: &Receiver<(u32, Event)>, pid: u32, limit: Duration, wanted: impl Fn(&Report) -> bool) -> Option<Report> {
    let deadline = Instant::now() + limit;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok((from, Event::Report(report))) if from == pid && wanted(&report) => return Some(report),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// The worker's exit code once it ended, within `limit`.
fn wait_end(worker: &mut Worker, limit: Duration) -> Option<Option<i32>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(code) = worker.ended() {
            return Some(code);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "starts `platinum mic-worker --simulate` processes (no audio device is opened)"]
fn a_held_worker_waits_for_go_then_stops_when_told() {
    let job = Job::new().unwrap();
    let (sender, events) = channel();
    let started = Instant::now();
    let (mut worker, unjobbed) = Worker::spawn(exe(), &simulated(Simulation::Quiet, true), Some(&job), sender).unwrap();
    assert_eq!(unjobbed, None);
    let pid = worker.pid();
    assert_eq!(wait_for(&events, pid, PATIENCE, |r| *r == Report::Engine(true)), Some(Report::Engine(true)));
    assert_eq!(wait_for(&events, pid, PATIENCE, |r| *r == Report::Ready), Some(Report::Ready));
    println!("ready {} ms after the start", started.elapsed().as_millis());
    // Held: it says nothing of streaming until told to go.
    assert_eq!(wait_for(&events, pid, Duration::from_millis(300), |r| matches!(r, Report::Streaming { .. })), None);
    let go = Instant::now();
    assert!(worker.send(Command::Go));
    assert_eq!(wait_for(&events, pid, PATIENCE, |r| matches!(r, Report::Streaming { .. })), Some(Report::Streaming { effects: true }));
    println!("streaming {} ms after go", go.elapsed().as_millis());
    // Its level comes twice a second, quiet.
    let level = wait_for(&events, pid, Duration::from_secs(2), |r| matches!(r, Report::Level { .. }));
    assert!(matches!(level, Some(Report::Level { peak, .. }) if peak < 0.01), "{level:?}");
    let stop = Instant::now();
    assert!(worker.send(Command::Stop));
    assert_eq!(wait_for(&events, pid, PATIENCE, |r| *r == Report::Stopped), Some(Report::Stopped));
    println!("stopped {} ms after stop", stop.elapsed().as_millis());
    assert_eq!(wait_end(&mut worker, PATIENCE), Some(Some(0)));
}

#[test]
#[ignore = "starts `platinum mic-worker --simulate` processes (no audio device is opened)"]
fn a_worker_ends_with_its_job_with_its_input_and_says_its_crash() {
    let (sender, events) = channel();
    // Closing the job ends the worker, as when the daemon ends, however it ends.
    let job = Job::new().unwrap();
    let (mut worker, _) = Worker::spawn(exe(), &simulated(Simulation::Loud, false), Some(&job), sender.clone()).unwrap();
    let loud = wait_for(&events, worker.pid(), PATIENCE, |r| matches!(r, Report::Level { .. }));
    assert!(matches!(loud, Some(Report::Level { peak, quiet }) if peak > 0.1 && quiet.is_zero()), "{loud:?}");
    drop(job);
    assert!(wait_end(&mut worker, Duration::from_secs(2)).is_some(), "closing the job did not end the worker");
    // Without a job, closing its input (the daemon gone) ends it.
    let (mut worker, _) = Worker::spawn(exe(), &simulated(Simulation::Loud, false), None, sender.clone()).unwrap();
    assert!(wait_for(&events, worker.pid(), PATIENCE, |r| matches!(r, Report::Streaming { .. })).is_some());
    worker.close_input();
    assert_eq!(wait_end(&mut worker, EXIT_GRACE + Duration::from_secs(2)), Some(Some(0)));
    // A crash is seen with its Windows status.
    let job = Job::new().unwrap();
    let (mut worker, _) = Worker::spawn(exe(), &simulated(Simulation::Crash, false), Some(&job), sender).unwrap();
    assert_eq!(wait_end(&mut worker, PATIENCE), Some(Some(0xC000_0005_u32 as i32)));
}
