use std::path::PathBuf;
use std::time::SystemTime;

use orbit_app::controller::AppController;
use orbit_app::messages::{AppEvent, Effect};
use orbit_domain::action::{ActionId, MENU_LAYOUT, MenuItem};
use orbit_domain::geometry::{MenuGeometry, ScreenPoint, ScreenRect};
use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};

use super::super::log::{FenceOutcome, Guard};
use super::*;

const A: GestureId = GestureId(1);
const B: GestureId = GestureId(2);
const HERE: QuartzPoint = QuartzPoint::new(700.0, 400.0);
const THERE: QuartzPoint = QuartzPoint::new(760.0, 380.0);
const SHORT: Duration = Duration::from_millis(20);

#[derive(Debug, Default)]
struct FakeIo {
    hid: HidKeys,
    posted: Vec<i64>,
    post_fails: bool,
    button: bool,
}

impl TapIo for FakeIo {
    fn keyboard_counter(&mut self) -> HidKeys {
        self.hid
    }

    fn button_held(&mut self) -> bool {
        self.button
    }

    fn post_marker(&mut self, user_data: i64) -> bool {
        if self.post_fails {
            return false;
        }
        self.posted.push(user_data);
        true
    }
}

/// A tracker with a fake system. Key helpers count HID events the way the
/// system does: before the tap sees them, without autorepeat.
struct Harness {
    log: Arc<InputLog>,
    tracker: GestureTracker,
    io: FakeIo,
    now: Instant,
}

impl Harness {
    fn new() -> Self {
        let log = Arc::new(InputLog::new());
        Self {
            tracker: GestureTracker::new(Arc::clone(&log)),
            log,
            io: FakeIo::default(),
            now: Instant::now(),
        }
    }

    fn send(&mut self, event: TapEvent) -> Outcome {
        self.tracker.on_event(event, self.now, &mut self.io)
    }

    fn signals(&mut self, event: TapEvent) -> Vec<Signal> {
        self.send(event).signals
    }

    fn key_down(&mut self, keycode: u16, modifiers: Modifiers) -> Outcome {
        self.io.hid.down += 1;
        self.send(TapEvent::KeyDown {
            keycode,
            autorepeat: false,
            modifiers,
            point: HERE,
        })
    }

    fn key_repeat(&mut self, keycode: u16, modifiers: Modifiers) -> Outcome {
        self.send(TapEvent::KeyDown {
            keycode,
            autorepeat: true,
            modifiers,
            point: HERE,
        })
    }

    fn key_up(&mut self, keycode: u16, point: QuartzPoint) -> Outcome {
        self.io.hid.up += 1;
        self.queued_key_up(keycode, point)
    }

    /// A key-up the HID counters counted earlier, such as one that waited
    /// in the queue of the tap behind the event that posted a marker.
    fn queued_key_up(&mut self, keycode: u16, point: QuartzPoint) -> Outcome {
        self.send(TapEvent::KeyUp { keycode, point })
    }

    fn hotkey(&mut self) -> Vec<Signal> {
        self.key_down(SPACE_KEYCODE, Modifiers::HOTKEY).signals
    }

    fn space_up(&mut self, point: QuartzPoint) -> Vec<Signal> {
        self.key_up(SPACE_KEYCODE, point).signals
    }

    fn modifiers_up(&mut self, point: QuartzPoint) -> Vec<Signal> {
        self.signals(TapEvent::FlagsChanged {
            modifiers: Modifiers::default(),
            point,
        })
    }

    /// Lets the tap see the markers posted so far, in order, and returns
    /// their signals. Every marker is swallowed.
    fn deliver(&mut self) -> Vec<Signal> {
        let mut signals = Vec::new();
        for user_data in std::mem::take(&mut self.io.posted) {
            let out = self.send(TapEvent::Scroll { user_data });
            assert_eq!(out.disposition, Disposition::Swallow);
            signals.extend(out.signals);
        }
        signals
    }

    fn poll(&mut self, poll: PollState) -> Vec<Signal> {
        self.tracker.on_poll(poll, self.now).signals
    }

    /// Runs a fence the way the resolver does and judges `gesture`.
    fn guard(&mut self, gesture: GestureId) -> Guard {
        let log = Arc::clone(&self.log);
        let Ok(ticket) = log.register_fence() else {
            panic!("fence not registered")
        };
        let out = self.send(TapEvent::Scroll {
            user_data: ticket.user_data(),
        });
        assert_eq!(out, swallowed());
        match ticket.wait(SHORT) {
            FenceOutcome::Seen(snapshot) => log.check(gesture, &snapshot),
            FenceOutcome::TimedOut => panic!("fence timed out"),
        }
    }

    /// Press, baseline marker seen.
    fn hold(&mut self) -> GestureId {
        let gesture = match self.hotkey().as_slice() {
            [Signal::Invoke { gesture, .. }] => *gesture,
            other => panic!("no invoke: {other:?}"),
        };
        assert_eq!(self.deliver(), vec![]);
        gesture
    }
}

const fn swallowed() -> Outcome {
    Outcome {
        disposition: Disposition::Swallow,
        signals: Vec::new(),
    }
}

const fn secure() -> PollState {
    PollState {
        secure_input: true,
        control: true,
        option: true,
    }
}

fn cancel_release(gesture: GestureId, point: QuartzPoint) -> Vec<Signal> {
    vec![
        Signal::Cancel { gesture },
        Signal::Release { gesture, point },
    ]
}

#[test]
fn gesture_released_by_space() {
    let mut h = Harness::new();
    assert_eq!(
        h.hotkey(),
        vec![Signal::Invoke {
            gesture: A,
            point: HERE
        }]
    );
    assert!(h.tracker.needs_poll());
    assert_eq!(h.deliver(), vec![]);
    assert_eq!(
        h.signals(TapEvent::MouseMoved(THERE)),
        vec![Signal::Move {
            gesture: A,
            point: THERE
        }]
    );
    assert_eq!(h.space_up(THERE), vec![]);
    assert_eq!(
        h.deliver(),
        vec![Signal::Release {
            gesture: A,
            point: THERE
        }]
    );
    assert!(!h.tracker.needs_poll());
    assert_eq!(h.guard(A), Guard::Clean);
}

#[test]
fn gesture_released_by_modifiers_with_keys_typed_around_it() {
    let mut h = Harness::new();
    h.key_down(0x00, Modifiers::default());
    h.key_up(0x00, HERE);
    h.hold();
    // Autorepeat of the hotkey and an arrow key during the hold.
    for _ in 0..5 {
        assert_eq!(
            h.key_repeat(SPACE_KEYCODE, Modifiers::HOTKEY),
            Outcome::pass()
        );
    }
    h.key_down(0x7D, Modifiers::HOTKEY);
    h.key_repeat(0x7D, Modifiers::HOTKEY);
    h.key_up(0x7D, HERE);
    // Space goes up right after the modifiers: counted before the release
    // marker is posted, seen by the tap before the marker.
    h.io.hid.up += 1;
    assert_eq!(h.modifiers_up(THERE), vec![]);
    assert_eq!(h.queued_key_up(SPACE_KEYCODE, THERE), Outcome::pass());
    assert_eq!(
        h.deliver(),
        vec![Signal::Release {
            gesture: A,
            point: THERE
        }]
    );
    // The arrow key changed the selection.
    assert_eq!(h.guard(A), Guard::Inputs);
}

#[test]
fn space_up_missed_by_the_tap_cancels_the_gesture() {
    let mut h = Harness::new();
    h.hold();
    // Secure keyboard entry hides Space-up from the tap; HID counts it.
    h.io.hid.up += 1;
    assert_eq!(h.modifiers_up(THERE), vec![]);
    assert_eq!(h.deliver(), cancel_release(A, THERE));
    assert!(!h.tracker.needs_poll());
    assert_eq!(h.guard(A), Guard::Interrupted);
}

#[test]
fn surplus_of_tap_events_cancels_the_gesture() {
    let mut h = Harness::new();
    h.hold();
    // A key event the HID counters did not count.
    h.send(TapEvent::KeyUp {
        keycode: 0x00,
        point: HERE,
    });
    h.space_up(HERE);
    assert_eq!(h.deliver(), cancel_release(A, HERE));
}

#[test]
fn late_release_marker_cancels_the_gesture() {
    let mut h = Harness::new();
    h.hold();
    h.space_up(THERE);
    h.now += RELEASE_TIMEOUT;
    assert_eq!(h.poll(secure()), cancel_release(A, THERE));
    // The marker that comes after the timeout is swallowed and does nothing.
    assert_eq!(h.deliver(), vec![]);
    assert_eq!(h.guard(A), Guard::Interrupted);

    // The same through the next event instead of the timer.
    let mut h = Harness::new();
    h.hold();
    h.space_up(THERE);
    h.now += RELEASE_TIMEOUT + SHORT;
    assert_eq!(
        h.signals(TapEvent::MouseMoved(HERE)),
        cancel_release(A, THERE)
    );
}

#[test]
fn disabled_tap_cancels_the_gesture() {
    let mut h = Harness::new();
    h.hold();
    assert_eq!(h.signals(TapEvent::TapDisabled), cancel_release(A, HERE));
    assert_eq!(h.guard(A), Guard::Interrupted);

    let mut h = Harness::new();
    h.hold();
    h.space_up(THERE);
    assert_eq!(h.signals(TapEvent::TapDisabled), cancel_release(A, THERE));
    assert_eq!(h.deliver(), vec![]);

    let mut h = Harness::new();
    assert_eq!(h.signals(TapEvent::TapDisabled), vec![]);
}

#[test]
fn new_press_ends_a_gesture_whose_release_was_lost() {
    let mut h = Harness::new();
    h.hold();
    let mut expected = cancel_release(A, HERE);
    expected.push(Signal::Invoke {
        gesture: B,
        point: HERE,
    });
    assert_eq!(h.hotkey(), expected);
    // The new gesture is not spoiled by the end of the old one.
    assert_eq!(h.deliver(), vec![]);
    assert_eq!(h.guard(B), Guard::Clean);

    let mut h = Harness::new();
    h.hold();
    h.space_up(THERE);
    let mut expected = cancel_release(A, THERE);
    expected.push(Signal::Invoke {
        gesture: B,
        point: HERE,
    });
    assert_eq!(h.hotkey(), expected);
    // The release marker of the old gesture is stale now.
    assert_eq!(h.deliver(), vec![]);
    assert!(h.tracker.needs_poll());
}

#[test]
fn hotkey_needs_exactly_control_and_option() {
    let mut h = Harness::new();
    let extra = Modifiers {
        command: true,
        ..Modifiers::HOTKEY
    };
    assert_eq!(h.key_down(SPACE_KEYCODE, extra), Outcome::pass());
    let only_control = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    assert_eq!(h.key_down(SPACE_KEYCODE, only_control), Outcome::pass());
    assert!(!h.tracker.needs_poll());
    assert!(!h.log.match_hotkey());
}

#[test]
fn escape_during_the_hold_is_swallowed_with_its_key_up() {
    let mut h = Harness::new();
    h.hold();
    assert_eq!(
        h.key_down(ESCAPE_KEYCODE, Modifiers::HOTKEY),
        Outcome {
            disposition: Disposition::Swallow,
            signals: vec![Signal::Cancel { gesture: A }],
        }
    );
    assert_eq!(h.key_repeat(ESCAPE_KEYCODE, Modifiers::HOTKEY), swallowed());
    // The press stays held until the release.
    assert_eq!(h.space_up(HERE), vec![]);
    assert_eq!(
        h.deliver(),
        vec![Signal::Release {
            gesture: A,
            point: HERE
        }]
    );
    // The key-up goes after the gesture has ended.
    assert_eq!(h.key_up(ESCAPE_KEYCODE, HERE), swallowed());
    // A swallowed Esc is no input.
    assert_eq!(h.guard(A), Guard::Clean);
}

#[test]
fn escape_outside_the_hold_passes() {
    let mut h = Harness::new();
    assert_eq!(
        h.key_down(ESCAPE_KEYCODE, Modifiers::default()),
        Outcome::pass()
    );
    assert_eq!(h.key_up(ESCAPE_KEYCODE, HERE), Outcome::pass());

    h.hold();
    // Esc pressed right after Space-up: counted before the release marker,
    // seen by the tap while it waits for the marker.
    h.io.hid.down += 1;
    h.io.hid.up += 1;
    h.space_up(HERE);
    assert_eq!(
        h.send(TapEvent::KeyDown {
            keycode: ESCAPE_KEYCODE,
            autorepeat: false,
            modifiers: Modifiers::HOTKEY,
            point: HERE,
        }),
        Outcome::pass()
    );
    assert_eq!(h.queued_key_up(ESCAPE_KEYCODE, HERE), Outcome::pass());
    assert_eq!(
        h.deliver(),
        vec![Signal::Release {
            gesture: A,
            point: HERE
        }]
    );
    assert_eq!(
        h.key_down(ESCAPE_KEYCODE, Modifiers::default()),
        Outcome::pass()
    );
    assert_eq!(h.key_up(ESCAPE_KEYCODE, HERE), Outcome::pass());
}

#[test]
fn watchdog_cancels_the_hold_it_cannot_see() {
    let mut h = Harness::new();
    assert_eq!(h.poll(secure()), vec![]);
    h.hold();
    assert_eq!(
        h.poll(PollState {
            secure_input: false,
            control: true,
            option: true,
        }),
        vec![]
    );
    assert_eq!(h.poll(secure()), cancel_release(A, HERE));
    assert!(!h.tracker.needs_poll());
    assert_eq!(h.poll(secure()), vec![]);
    assert_eq!(h.guard(A), Guard::Interrupted);
    // The next press starts afresh.
    assert_eq!(
        h.hotkey(),
        vec![Signal::Invoke {
            gesture: B,
            point: HERE
        }]
    );

    let mut h = Harness::new();
    h.hold();
    let released = PollState {
        secure_input: false,
        control: true,
        option: false,
    };
    assert_eq!(h.poll(released), cancel_release(A, HERE));
}

#[test]
fn drags_and_held_buttons_spoil_the_selection() {
    let mut h = Harness::new();
    h.send(TapEvent::MouseDown(Button::Left));
    h.hold();
    assert_eq!(h.guard(A), Guard::ButtonHeld);
    h.send(TapEvent::MouseUp(Button::Left));
    h.space_up(HERE);
    h.deliver();

    // A button the HID state has down though the tap saw no mouse-down.
    let mut h = Harness::new();
    h.io.button = true;
    h.hold();
    assert_eq!(h.guard(A), Guard::ButtonHeld);

    let mut h = Harness::new();
    h.hold();
    assert_eq!(
        h.signals(TapEvent::MouseDragged(THERE)),
        vec![Signal::Move {
            gesture: A,
            point: THERE
        }]
    );
    assert_eq!(h.guard(A), Guard::Inputs);

    let mut h = Harness::new();
    h.hold();
    h.send(TapEvent::MouseUp(Button::Right));
    assert_eq!(h.guard(A), Guard::Inputs);
}

#[test]
fn markers_are_always_swallowed() {
    let mut h = Harness::new();
    let unknown = super::super::marker_user_data(12_345);
    assert_eq!(h.send(TapEvent::Scroll { user_data: unknown }), swallowed());
    // A real scroll passes and is no input.
    h.hold();
    assert_eq!(h.send(TapEvent::Scroll { user_data: 0 }), Outcome::pass());
    assert_eq!(h.guard(A), Guard::Clean);

    // A fence marker after its wait timed out.
    let log = Arc::clone(&h.log);
    let Ok(ticket) = log.register_fence() else {
        panic!("fence not registered")
    };
    let user_data = ticket.user_data();
    assert_eq!(ticket.wait(Duration::ZERO), FenceOutcome::TimedOut);
    assert_eq!(h.send(TapEvent::Scroll { user_data }), swallowed());
}

#[test]
fn fences_outlive_a_restart_of_the_tap() {
    let mut h = Harness::new();
    let log = Arc::clone(&h.log);
    let Ok(lost) = log.register_fence() else {
        panic!("fence not registered")
    };
    // The tap is removed and created again; its marker never arrives.
    h.send(TapEvent::TapDisabled);
    h.tracker = GestureTracker::new(Arc::clone(&log));
    assert_eq!(lost.wait(SHORT), FenceOutcome::TimedOut);

    h.hold();
    assert_eq!(h.guard(A), Guard::Clean);
}

#[test]
fn failed_markers_cancel_the_gesture() {
    let mut h = Harness::new();
    h.io.post_fails = true;
    assert_eq!(
        h.hotkey(),
        vec![
            Signal::Invoke {
                gesture: A,
                point: HERE
            },
            Signal::Cancel { gesture: A },
        ]
    );
    assert_eq!(h.space_up(THERE), cancel_release(A, THERE));
    assert!(!h.tracker.needs_poll());

    let mut h = Harness::new();
    h.hold();
    h.io.post_fails = true;
    assert_eq!(h.space_up(THERE), cancel_release(A, THERE));
}

#[test]
fn gesture_ids_wrap() {
    let mut h = Harness::new();
    h.tracker.next_gesture = u64::MAX;
    assert_eq!(h.hold(), GestureId(u64::MAX));
    h.space_up(HERE);
    h.deliver();
    assert_eq!(h.hold(), GestureId(0));
}

#[test]
fn every_press_is_counted_for_carbon_once() {
    let mut h = Harness::new();
    h.hold();
    h.key_repeat(SPACE_KEYCODE, Modifiers::HOTKEY);
    h.hotkey();
    assert!(h.log.match_hotkey());
    assert!(h.log.match_hotkey());
    assert!(!h.log.match_hotkey());
}

// The tracker and the controller together: a gesture over Copy Path writes
// the clipboard only when the release is confirmed.

fn file(path: &str) -> FileTarget {
    FileTarget {
        path: PathBuf::from(path),
        identity: FileIdentity {
            device: 1,
            inode: 42,
        },
        version: FileVersion {
            size: 1024,
            modified: SystemTime::UNIX_EPOCH,
        },
        facts: FileFacts {
            kind: FileKind::File,
            image: None,
        },
    }
}

/// Feeds signals to the controller the way the hotkey adapter will, with
/// Quartz points taken as screen points, and answers the resolver at once.
fn drive(controller: &mut AppController, signals: Vec<Signal>) -> Vec<Effect> {
    let point = |p: QuartzPoint| ScreenPoint::new(p.x, p.y);
    let mut effects = Vec::new();
    for signal in signals {
        let event = match signal {
            Signal::Invoke { gesture, point: at } => AppEvent::Invoked {
                gesture,
                point: point(at),
                screens: vec![ScreenRect::new(0.0, 0.0, 1440.0, 875.0)],
            },
            Signal::Move { gesture, point: at } => AppEvent::PointerMoved {
                gesture,
                point: point(at),
            },
            Signal::Release { gesture, point: at } => AppEvent::Released {
                gesture,
                point: point(at),
            },
            Signal::Cancel { gesture } => AppEvent::GestureCancelled { gesture },
        };
        for effect in controller.update(event) {
            if let Effect::ResolveTarget { gesture } = effect {
                effects.extend(controller.update(AppEvent::TargetResolved {
                    gesture,
                    result: Ok(file("/tmp/a.txt")),
                }));
            } else {
                effects.push(effect);
            }
        }
    }
    effects
}

/// A point over Copy Path of a menu centered at `center`.
fn over_copy_path(center: ScreenPoint) -> QuartzPoint {
    let slot = MENU_LAYOUT
        .iter()
        .position(|item| *item == MenuItem::Action(ActionId::CopyPath))
        .unwrap_or_default();
    let angle = MenuGeometry::standard().slot_center(slot);
    QuartzPoint::new(center.x + 70.0 * angle.sin(), center.y + 70.0 * angle.cos())
}

fn gesture_over_copy_path(lose_space_up: bool) -> Vec<Effect> {
    let mut h = Harness::new();
    let mut controller = AppController::new();
    let mut effects = drive(&mut controller, h.hotkey());
    let center = match effects.as_slice() {
        [Effect::PresentMenu(model)] => model.center,
        other => panic!("menu not presented: {other:?}"),
    };
    effects.extend(drive(&mut controller, h.deliver()));
    let target = over_copy_path(center);
    let moved = h.signals(TapEvent::MouseMoved(target));
    effects.extend(drive(&mut controller, moved));
    if lose_space_up {
        h.io.hid.up += 1;
    } else {
        h.key_up(SPACE_KEYCODE, target);
    }
    let released = h.modifiers_up(target);
    effects.extend(drive(&mut controller, released));
    effects.extend(drive(&mut controller, h.deliver()));
    effects
}

#[test]
fn confirmed_release_over_copy_path_writes_the_clipboard() {
    let effects = gesture_over_copy_path(false);
    assert!(
        effects.contains(&Effect::WriteClipboard("/tmp/a.txt".into())),
        "{effects:?}"
    );
}

#[test]
fn release_after_a_missed_space_up_writes_nothing() {
    let effects = gesture_over_copy_path(true);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::WriteClipboard(_))),
        "{effects:?}"
    );
    assert!(effects.contains(&Effect::HideMenu), "{effects:?}");
}
