//! Delivery through the real main dispatch queue.
//!
//! Runs without the test harness so that `main` is the process main thread
//! and the main run loop can be pumped here.

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use orbit_app::messages::{AppEvent, GestureId};
use orbit_app::ports::EventSink;
use orbit_macos::main_thread::{MainThreadError, MainThreadSender};

const SENDERS: u64 = 3;
const PER_SENDER: u64 = 200;
const ROUNDS: u64 = 100;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Ids of the per-sender events: `sender * 1000 + index`.
const SENDER_BASE: u64 = 1_000;
/// Ids of the cross-thread pairs: `A_BASE + round` then `B_BASE + round`.
const A_BASE: u64 = 100_000;
const B_BASE: u64 = 200_000;
/// Ids of the events sent before the handler exists: `EARLY_BASE + index`.
const EARLY_BASE: u64 = 300_000;
const EARLY: u64 = 20;

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("test main_thread ... ok");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("test main_thread ... FAILED: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let sender = MainThreadSender::new(mtm).map_err(|err| err.to_string())?;
    if MainThreadSender::new(mtm).map(|_| ()) != Err(MainThreadError::AlreadyCreated) {
        return Err("a second sender was created".into());
    }

    // Events sent before the handler exists are kept and come first, in the
    // order they were sent: the first from this thread, the rest from
    // another thread after the first send had returned.
    sender.send(cancelled(EARLY_BASE));
    thread::spawn(move || {
        for index in 1..EARLY {
            sender.send(cancelled(EARLY_BASE + index));
        }
    })
    .join()
    .map_err(|_| "the early sender thread panicked")?;
    // The main queue is serial: when this block runs, the deliveries queued
    // above have run, and found no handler.
    let drained = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&drained);
    DispatchQueue::main().exec_async(move || flag.store(true, Ordering::Release));
    pump_until(|| drained.load(Ordering::Acquire), "the early deliveries")?;

    let seen = Rc::new(RefCell::new(Vec::new()));
    let off_main = Rc::new(RefCell::new(0_u32));
    let (log, stray) = (Rc::clone(&seen), Rc::clone(&off_main));
    sender
        .set_handler(mtm, move |event| {
            if MainThreadMarker::new().is_none() {
                *stray.borrow_mut() += 1;
            }
            if let AppEvent::GestureCancelled { gesture } = event {
                log.borrow_mut().push(gesture.0);
            }
        })
        .map_err(|err| err.to_string())?;
    // `set_handler` hands over the kept events at once, on this thread.
    let early: Vec<u64> = (0..EARLY).map(|index| EARLY_BASE + index).collect();
    if *seen.borrow() != early {
        return Err(format!(
            "events kept before the handler: {:?}, expected {early:?}",
            seen.borrow()
        ));
    }

    let mut threads = Vec::new();
    for id in 0..SENDERS {
        threads.push(thread::spawn(move || {
            for index in 0..PER_SENDER {
                sender.send(cancelled(id * SENDER_BASE + index));
            }
        }));
    }
    // `a` is sent and returns before `b` starts, from another thread.
    let (sent_a, a_sent) = mpsc::channel::<u64>();
    threads.push(thread::spawn(move || {
        for round in 0..ROUNDS {
            sender.send(cancelled(A_BASE + round));
            if sent_a.send(round).is_err() {
                return;
            }
        }
    }));
    threads.push(thread::spawn(move || {
        while let Ok(round) = a_sent.recv() {
            sender.send(cancelled(B_BASE + round));
        }
    }));

    let expected =
        usize::try_from(EARLY + SENDERS * PER_SENDER + 2 * ROUNDS).map_err(|e| e.to_string())?;
    pump_until(
        || seen.borrow().len() >= expected,
        &format!("{expected} events"),
    )?;
    for handle in threads {
        handle.join().map_err(|_| "a sender thread panicked")?;
    }

    if *off_main.borrow() != 0 {
        return Err(format!(
            "{} events handled off the main thread",
            off_main.borrow()
        ));
    }
    let seen = seen.borrow();
    if seen.len() != expected {
        return Err(format!("{} events, expected {expected}", seen.len()));
    }
    for id in 0..SENDERS {
        let order: Vec<u64> = seen
            .iter()
            .copied()
            .filter(|&value| value < A_BASE && value / SENDER_BASE == id)
            .collect();
        let sent: Vec<u64> = (0..PER_SENDER)
            .map(|index| id * SENDER_BASE + index)
            .collect();
        if order != sent {
            return Err(format!("sender {id}: events out of order"));
        }
    }
    for round in 0..ROUNDS {
        let position = |value| seen.iter().position(|&seen| seen == value);
        match (position(A_BASE + round), position(B_BASE + round)) {
            (Some(a), Some(b)) if a < b => {}
            other => return Err(format!("round {round}: a and b at {other:?}")),
        }
    }
    Ok(())
}

/// Runs the main run loop until `done`, or fails after [`TIMEOUT`].
fn pump_until(done: impl Fn() -> bool, what: &str) -> Result<(), String> {
    let started = Instant::now();
    while !done() {
        if started.elapsed() > TIMEOUT {
            return Err(format!("{what} not delivered in {TIMEOUT:?}"));
        }
        // SAFETY: reading an immutable framework constant.
        let mode = unsafe { kCFRunLoopDefaultMode };
        CFRunLoop::run_in_mode(mode, 0.05, false);
    }
    Ok(())
}

fn cancelled(id: u64) -> AppEvent {
    AppEvent::GestureCancelled {
        gesture: GestureId(id),
    }
}
