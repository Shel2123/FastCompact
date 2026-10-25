//! Selection mode (I2-default): the target is exactly one selected item in
//! the Finder file area that has keyboard focus. Read once per gesture on the
//! resolver thread; the cursor is never used.
//!
//! The file area is recognised by the container's role and identifiers, not
//! by the presence of a file URL: the search field, the sidebar and anything
//! unknown are refused even when they expose `AXURL`.

use std::fmt;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGEventType};
use tracing::{info, warn};

use crate::applescript;
use crate::ax::{self, Event};
use crate::tap;

pub struct Request {
    pub session: u64,
    pub finder_pid: i32,
    /// Best known time of the hotkey press: the event tap's key-down when
    /// available, otherwise the hotkey handler.
    pub pressed_at: Instant,
    /// Delay before the read (scenario D11).
    pub delay: Duration,
}

/// Second read after the panel is shown, to check that the panel changes
/// neither focus nor selection (scenario D12). Diagnostic only.
pub struct Verify {
    pub session: u64,
    pub finder_pid: i32,
    pub expected: Snapshot,
}

#[derive(Debug)]
pub enum Outcome {
    Resolved(PathBuf),
    NoSelection,
    /// More than one item is selected. `paths` is empty when there are too
    /// many to read.
    Multiple {
        count: usize,
        paths: Vec<PathBuf>,
    },
    /// One selected item exposes several different file URLs.
    Ambiguous(Vec<PathBuf>),
    Unsupported(String),
    /// Mouse or keyboard input between the press and the end of the read.
    Unstable(String),
    PermissionDenied,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolved(path) => write!(f, "Resolved({})", path.display()),
            Self::NoSelection => write!(f, "NoSelection"),
            Self::Multiple { count, paths } => write!(f, "Multiple({count}: {paths:?})"),
            Self::Ambiguous(paths) => write!(f, "Ambiguous({paths:?})"),
            Self::Unsupported(reason) => write!(f, "Unsupported({reason})"),
            Self::Unstable(inputs) => write!(f, "Unstable({inputs})"),
            Self::PermissionDenied => write!(f, "PermissionDenied"),
        }
    }
}

/// What the focused part of Finder looked like at the read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    window: Option<String>,
    focus_role: Option<String>,
    area: Option<String>,
    /// Selected paths, sorted.
    paths: Vec<PathBuf>,
}

impl Snapshot {
    /// Paths for the AppleScript cross-check, or `None` when the read did not
    /// produce a complete selection.
    fn comparable_paths(&self, outcome: &Outcome) -> Option<Vec<PathBuf>> {
        match outcome {
            Outcome::Resolved(_) | Outcome::NoSelection => Some(self.paths.clone()),
            Outcome::Multiple { count, .. } if *count == self.paths.len() => {
                Some(self.paths.clone())
            }
            _ => None,
        }
    }
}

/// Wait this long after the read before checking for input, so that events
/// that happened during the read have reached the event tap.
const GUARD_GRACE: Duration = Duration::from_millis(30);
/// Delay before the verification read, so the panel is on screen.
const VERIFY_DELAY: Duration = Duration::from_millis(100);
/// Read the paths of at most this many selected items.
const MAX_SELECTED: usize = 200;
/// Scroll areas allowed between a column list and its browser.
const MAX_COLUMN_SCROLL_AREAS: usize = 4;

pub fn resolve(request: &Request, sink: &impl Fn(Event), checker: Option<&applescript::Checker>) {
    let session = request.session;
    thread::sleep(request.delay);

    let started = Instant::now();
    let (outcome, snapshot) = read(session, request.finder_pid, "read");
    let elapsed = started.elapsed();

    thread::sleep(GUARD_GRACE);
    let outcome = match input_after(session, request.pressed_at) {
        Some(inputs) => {
            info!(session, read = %outcome, %inputs, "input during read; selection rejected");
            Outcome::Unstable(inputs)
        }
        None => outcome,
    };
    let comparable = snapshot.comparable_paths(&outcome);
    sink(Event::Selected {
        session,
        finder_pid: request.finder_pid,
        outcome,
        snapshot,
        elapsed,
    });
    if let Some(checker) = checker {
        checker.submit(applescript::Check {
            session,
            ax: comparable,
        });
    }
}

pub fn verify(request: &Verify) {
    thread::sleep(VERIFY_DELAY);
    let (outcome, snapshot) = read(request.session, request.finder_pid, "verify");
    info!(
        session = request.session,
        same = snapshot == request.expected,
        %outcome,
        ?snapshot,
        expected = ?request.expected,
        "panel focus check"
    );
}

/// The known Finder views that hold files.
#[derive(Debug, Clone, Copy)]
enum Area {
    List,
    Collection,
    Column,
    Desktop,
}

enum Step {
    Area(Area),
    Refuse(String),
    Up,
}

/// Attributes that identify a container.
struct Info {
    role: Option<String>,
    subrole: Option<String>,
    identifier: Option<String>,
    description: Option<String>,
}

impl Info {
    fn read(element: &AXUIElement) -> Self {
        Self {
            role: ax::string_attribute(element, "AXRole"),
            subrole: ax::string_attribute(element, "AXSubrole"),
            identifier: ax::string_attribute(element, "AXIdentifier"),
            description: ax::string_attribute(element, "AXDescription"),
        }
    }

    fn is(&self, role: &str) -> bool {
        self.role.as_deref() == Some(role)
    }
}

fn read(session: u64, pid: i32, phase: &'static str) -> (Outcome, Snapshot) {
    // SAFETY: any pid is accepted; for a pid without an app the calls fail.
    let app = unsafe { AXUIElement::new_application(pid) };
    // SAFETY: `app` is a valid element.
    let err = unsafe { app.set_messaging_timeout(ax::MESSAGING_TIMEOUT_SECS) };
    if err != AXError::Success {
        warn!(session, ?err, "set_messaging_timeout failed");
    }
    let mut snapshot = Snapshot {
        window: element_attribute(&app, "AXFocusedWindow")
            .and_then(|window| ax::string_attribute(&window, "AXTitle")),
        ..Snapshot::default()
    };

    let focus = match ax::attribute(&app, "AXFocusedUIElement") {
        Err(AXError::APIDisabled) => return (Outcome::PermissionDenied, snapshot),
        Err(err) => {
            let reason = format!("focused element unavailable: {err:?}");
            return (Outcome::Unsupported(reason), snapshot);
        }
        Ok(None) => {
            let reason = "Finder reports no focused element".to_owned();
            return (Outcome::Unsupported(reason), snapshot);
        }
        // SAFETY: AXFocusedUIElement always holds an AXUIElement.
        Ok(Some(value)) => unsafe { CFRetained::cast_unchecked::<AXUIElement>(value) },
    };

    let (area, container) = match find_area(session, phase, focus, &mut snapshot) {
        Ok(found) => found,
        Err(reason) => return (Outcome::Unsupported(reason), snapshot),
    };
    snapshot.area = Some(format!("{area:?}"));

    let items = match selected_items(area, &container) {
        Ok(items) => items,
        Err(reason) => return (Outcome::Unsupported(reason), snapshot),
    };
    if items.len() > MAX_SELECTED {
        let outcome = Outcome::Multiple {
            count: items.len(),
            paths: Vec::new(),
        };
        return (outcome, snapshot);
    }

    let mut paths = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let role = ax::string_attribute(item, "AXRole");
        ax::log_element(session, "selected", index, item, role.as_deref(), false);
        let found = item_paths(session, item);
        match found.len() {
            0 => {
                let reason = format!("selected item {index} ({role:?}) has no file URL");
                return (Outcome::Unsupported(reason), snapshot);
            }
            1 => paths.extend(found),
            _ => return (Outcome::Ambiguous(found), snapshot),
        }
    }
    let mut sorted = paths.clone();
    sorted.sort();
    snapshot.paths = sorted;

    let outcome = match paths.len() {
        0 => Outcome::NoSelection,
        1 => Outcome::Resolved(paths.remove(0)),
        count => Outcome::Multiple { count, paths },
    };
    (outcome, snapshot)
}

/// Walks up from the focused element to the file area that holds it.
fn find_area(
    session: u64,
    phase: &'static str,
    focus: CFRetained<AXUIElement>,
    snapshot: &mut Snapshot,
) -> Result<(Area, CFRetained<AXUIElement>), String> {
    let mut current = Some(focus);
    for depth in 0..ax::MAX_DEPTH {
        let Some(element) = current.take() else {
            break;
        };
        let info = Info::read(&element);
        if depth == 0 {
            snapshot.focus_role.clone_from(&info.role);
        }
        ax::log_element(
            session,
            phase,
            depth,
            &element,
            info.role.as_deref(),
            depth == 0,
        );
        match classify(&element, &info) {
            Step::Area(area) => {
                ax::log_element(session, "area", depth, &element, info.role.as_deref(), true);
                return Ok((area, element));
            }
            Step::Refuse(reason) => return Err(format!("{reason} (focus depth {depth})")),
            Step::Up => {}
        }
        current = ax::parent(&element);
    }
    Err("no Finder file area above the focused element".to_owned())
}

fn classify(element: &AXUIElement, info: &Info) -> Step {
    let subrole = info.subrole.as_deref();
    let identifier = info.identifier.as_deref();
    let description = info.description.as_deref();
    match info.role.as_deref().unwrap_or_default() {
        "AXTextField" if subrole == Some("AXSearchField") => {
            Step::Refuse("focus in the search field".to_owned())
        }
        "AXOutline" if identifier == Some("ListView") => Step::Area(Area::List),
        "AXOutline" if description == Some("sidebar") => {
            Step::Refuse("focus in the sidebar".to_owned())
        }
        "AXList"
            if subrole == Some("AXCollectionList")
                && matches!(identifier, Some("IconView" | "GalleryView")) =>
        {
            Step::Area(Area::Collection)
        }
        // A section of an icon or gallery view; its collection list is above.
        "AXList" if subrole == Some("AXSectionList") => Step::Up,
        "AXList" if in_column_view(element) => Step::Area(Area::Column),
        "AXBrowser" if identifier == Some("ColumnView") => {
            Step::Refuse("focus on the column browser itself; active column unknown".to_owned())
        }
        "AXGroup" if description == Some("desktop") && is_desktop_scroll_area(element) => {
            Step::Area(Area::Desktop)
        }
        role @ ("AXOutline" | "AXList" | "AXTable" | "AXBrowser") => Step::Refuse(format!(
            "focus in an unknown {role} (subrole {subrole:?}, identifier {identifier:?}, \
             description {description:?})"
        )),
        role @ ("AXWindow" | "AXSheet" | "AXApplication") => {
            Step::Refuse(format!("no file area between the focus and {role}"))
        }
        _ => Step::Up,
    }
}

/// A column list: `AXBrowser` (ColumnView) → one or more `AXScrollArea` →
/// `AXList`. Finder nests two or three scroll areas.
fn in_column_view(list: &AXUIElement) -> bool {
    let mut current = ax::parent(list);
    let mut scroll_areas = 0;
    while let Some(element) = current {
        let info = Info::read(&element);
        if info.is("AXScrollArea") && scroll_areas < MAX_COLUMN_SCROLL_AREAS {
            scroll_areas += 1;
            current = ax::parent(&element);
            continue;
        }
        return scroll_areas > 0
            && info.is("AXBrowser")
            && info.identifier.as_deref() == Some("ColumnView");
    }
    false
}

fn is_desktop_scroll_area(group: &AXUIElement) -> bool {
    ax::parent(group).is_some_and(|scroll| {
        let info = Info::read(&scroll);
        info.is("AXScrollArea") && info.description.as_deref() == Some("desktop")
    })
}

fn selected_items(
    area: Area,
    container: &AXUIElement,
) -> Result<Vec<CFRetained<AXUIElement>>, String> {
    let read = |element: &AXUIElement, name: &str| match ax::element_array(element, name) {
        Ok(Some(items)) => Ok(items),
        Ok(None) => Err(format!("{area:?} area has no {name}")),
        Err(err) => Err(format!("{area:?} area: {name} unavailable: {err:?}")),
    };
    match area {
        Area::List => read(container, "AXSelectedRows"),
        Area::Column | Area::Desktop => read(container, "AXSelectedChildren"),
        Area::Collection => {
            // The collection list may report the selection itself or leave it
            // to its section lists; both belong to the same view.
            if let Ok(Some(items)) = ax::element_array(container, "AXSelectedChildren")
                && !items.is_empty()
            {
                return Ok(items);
            }
            let mut items = Vec::new();
            for section in ax::element_array(container, "AXChildren")
                .ok()
                .flatten()
                .unwrap_or_default()
            {
                if Info::read(&section).subrole.as_deref() == Some("AXSectionList") {
                    items.extend(read(&section, "AXSelectedChildren")?);
                }
            }
            Ok(items)
        }
    }
}

/// A selected item stands for one file: its own URL, or the URL of one of
/// its descendants (list rows keep it on the name cell's text field).
fn item_paths(session: u64, item: &AXUIElement) -> Vec<PathBuf> {
    if let Some(path) = ax::file_url_attribute(item, "AXURL") {
        return vec![path];
    }
    let mut paths = Vec::new();
    let mut budget = ax::MAX_DESCENDANTS;
    ax::collect_descendant_urls(session, item, 1, &mut budget, &mut paths);
    paths
}

fn element_attribute(element: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    let value = ax::attribute(element, name).ok()??;
    // SAFETY: only called for attributes that hold an AXUIElement.
    Some(unsafe { CFRetained::cast_unchecked::<AXUIElement>(value) })
}

/// Input since the press that could have changed the selection. The event
/// tap gives exact events when it runs; the HID state only tells the time
/// of the last mouse-down. Key-downs from HID are logged, not used: holding
/// Space produces autorepeats.
fn input_after(session: u64, pressed_at: Instant) -> Option<String> {
    let mut seen = Vec::new();
    let tap_running = tap::running();
    if tap_running {
        for input in tap::inputs_since(pressed_at) {
            let after = input.seen_at.duration_since(pressed_at);
            seen.push(format!("tap {:?} +{after:?}", input.input));
        }
    }
    let now = Instant::now();
    let last = |event_type| {
        let secs = CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::CombinedSessionState,
            event_type,
        );
        Duration::try_from_secs_f64(secs)
            .ok()
            .and_then(|ago| now.checked_sub(ago))
    };
    for (name, event_type) in [
        ("left mouse-down", CGEventType::LeftMouseDown),
        ("right mouse-down", CGEventType::RightMouseDown),
        ("other mouse-down", CGEventType::OtherMouseDown),
    ] {
        if let Some(at) = last(event_type)
            && at > pressed_at
        {
            seen.push(format!("hid {name} +{:?}", at.duration_since(pressed_at)));
        }
    }
    let key_down = last(CGEventType::KeyDown).map(|at| {
        if at > pressed_at {
            format!("+{:?} after press", at.duration_since(pressed_at))
        } else {
            format!("{:?} before press", pressed_at.duration_since(at))
        }
    });
    info!(session, tap_running, ?seen, hid_key_down = ?key_down, "guard");
    (!seen.is_empty()).then(|| seen.join(", "))
}
