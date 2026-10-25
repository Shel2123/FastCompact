//! Resolver thread. Cursor mode: system-wide AX hit-test at a fixed point,
//! then an attribute walk from the hit element towards the window. Selection
//! mode is in `selection.rs` and runs on the same thread.
//!
//! `AXUIElement` is not `Send`, so all AX work runs on this thread and only
//! plain Rust values are sent back.

use std::fmt;
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType, CFURL};
use tracing::{error, info};

use crate::applescript;
use crate::selection;

pub enum Request {
    Cursor(CursorRequest),
    Selection(selection::Request),
    Verify(selection::Verify),
}

/// Cursor-mode request. `point` is in Quartz global coordinates and never
/// changes after the hotkey press.
pub struct CursorRequest {
    pub session: u64,
    pub point: (f64, f64),
    pub finder_pids: Vec<i32>,
    pub hit_delay: Duration,
    pub attr_delay: Duration,
}

#[derive(Debug)]
pub enum Event {
    /// Phase 1 finished: the top-most element at the point is known.
    HitTested {
        session: u64,
        result: Result<Hit, String>,
        elapsed: Duration,
    },
    /// Phase 2 finished.
    Resolved {
        session: u64,
        resolution: Resolution,
        elapsed: Duration,
    },
    /// Selection mode: the selection was read once for this gesture.
    Selected {
        session: u64,
        finder_pid: i32,
        outcome: selection::Outcome,
        snapshot: selection::Snapshot,
        elapsed: Duration,
    },
}

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only logged through Debug")]
pub struct Hit {
    pub pid: i32,
    pub role: Option<String>,
}

#[derive(Debug)]
#[expect(dead_code, reason = "fields are only logged through Debug")]
pub enum Resolution {
    Resolved { path: PathBuf, source: String },
    Ambiguous(Vec<PathBuf>),
    Unsupported(String),
    PermissionDenied,
}

/// Walk at most this many levels above the hit element.
pub(crate) const MAX_DEPTH: usize = 12;
/// Search at most this many levels below the item root.
const MAX_DESCENT: usize = 3;
/// Inspect at most this many descendants of the item root.
pub(crate) const MAX_DESCENDANTS: usize = 40;
pub(crate) const MESSAGING_TIMEOUT_SECS: f32 = 0.5;

/// Roles that hold many items. Reaching one without a file URL means the
/// point is not over a single item.
const CONTAINER_ROLES: &[&str] = &[
    "AXApplication",
    "AXBrowser",
    "AXList",
    "AXOutline",
    "AXScrollArea",
    "AXSheet",
    "AXSplitGroup",
    "AXTable",
    "AXWindow",
];

/// Item roots whose descendants may be searched for the file URL.
const ITEM_ROOT_ROLES: &[&str] = &["AXRow"];

/// Attributes logged for every element of the walk.
const DUMP_ATTRIBUTES: &[&str] = &[
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXIdentifier",
    "AXFilename",
    "AXURL",
    "AXDocument",
    "AXValue",
    "AXSelected",
];

pub fn spawn(
    sink: impl Fn(Event) + Send + 'static,
    checker: Option<applescript::Checker>,
) -> mpsc::Sender<Request> {
    let (sender, receiver) = mpsc::channel::<Request>();
    let spawned = thread::Builder::new()
        .name("orbit-spike-resolver".into())
        .spawn(move || {
            // SAFETY: creating the system-wide element has no preconditions.
            let system = unsafe { AXUIElement::new_system_wide() };
            // SAFETY: `system` is a valid element.
            let err = unsafe { system.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
            if err != AXError::Success {
                error!(?err, "set_messaging_timeout failed");
            }
            for request in receiver {
                match request {
                    Request::Cursor(request) => resolve(&system, &request, &sink),
                    Request::Selection(request) => {
                        selection::resolve(&request, &sink, checker.as_ref());
                    }
                    Request::Verify(request) => selection::verify(&request),
                }
            }
        });
    if let Err(err) = spawned {
        error!(%err, "resolver thread not started");
    }
    sender
}

fn resolve(system: &AXUIElement, request: &CursorRequest, sink: &impl Fn(Event)) {
    let session = request.session;
    thread::sleep(request.hit_delay);

    let started = Instant::now();
    let hit = match element_at(system, request.point) {
        Ok(element) => element,
        Err(err) => {
            let reason = if err == AXError::APIDisabled {
                "accessibility access denied".to_owned()
            } else {
                format!("no element at point: {err:?}")
            };
            sink(Event::HitTested {
                session,
                result: Err(reason),
                elapsed: started.elapsed(),
            });
            return;
        }
    };
    let pid = element_pid(&hit);
    let role = string_attribute(&hit, "AXRole");
    let result = match pid {
        Some(pid) if request.finder_pids.contains(&pid) => Ok(Hit { pid, role }),
        Some(pid) => Err(format!(
            "element belongs to pid {pid}, not Finder; role {role:?}"
        )),
        None => Err("element pid unavailable".to_owned()),
    };
    let is_finder = result.is_ok();
    sink(Event::HitTested {
        session,
        result,
        elapsed: started.elapsed(),
    });
    if !is_finder {
        return;
    }

    thread::sleep(request.attr_delay);
    let started = Instant::now();
    let resolution = walk(session, hit);
    sink(Event::Resolved {
        session,
        resolution,
        elapsed: started.elapsed(),
    });
}

/// Logs the chain from the hit element upwards, then looks for a file URL
/// on the item under the cursor: first on the chain itself, then, when the
/// item root (the top-most element below the first container) is a row,
/// among its descendants. Nothing above the item root is ever used.
fn walk(session: u64, hit: CFRetained<AXUIElement>) -> Resolution {
    let mut item_chain = Vec::new();
    let mut container: Option<(Option<String>, usize)> = None;
    let mut current = Some(hit);
    for depth in 0..MAX_DEPTH {
        let Some(element) = current.take() else {
            break;
        };
        let role = match attribute(&element, "AXRole") {
            Err(AXError::APIDisabled) => return Resolution::PermissionDenied,
            Ok(value) => value.and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string())),
            Err(_) => None,
        };
        let is_container = role
            .as_deref()
            .is_some_and(|role| CONTAINER_ROLES.contains(&role));
        let below_container = container.is_none() && !is_container;
        log_element(
            session,
            "up",
            depth,
            &element,
            role.as_deref(),
            below_container,
        );
        if container.is_none() {
            if is_container {
                container = Some((role.clone(), depth));
            } else {
                item_chain.push(element.clone());
            }
        }
        if role.as_deref() == Some("AXApplication") {
            break;
        }
        current = parent(&element);
    }

    for (depth, element) in item_chain.iter().enumerate() {
        if let Some(path) = file_url_attribute(element, "AXURL") {
            return Resolution::Resolved {
                path,
                source: format!("own AXURL at depth {depth}"),
            };
        }
    }

    let Some(item_root) = item_chain.last() else {
        return Resolution::Unsupported(format!("hit element is a container: {container:?}"));
    };
    // Only a row is known to stand for exactly one file. Other groups (for
    // example the desktop's AXGroup) may hold many items.
    let root_role = string_attribute(item_root, "AXRole");
    if !root_role
        .as_deref()
        .is_some_and(|role| ITEM_ROOT_ROLES.contains(&role))
    {
        // Diagnostic only: log what the group holds, never use it as a target.
        let mut seen = Vec::new();
        let mut budget = MAX_DESCENDANTS;
        collect_descendant_urls(session, item_root, 1, &mut budget, &mut seen);
        return Resolution::Unsupported(format!(
            "no own file URL and item root {root_role:?} is not a row; \
             container {container:?}; descendants hold {seen:?}"
        ));
    }
    let mut paths = Vec::new();
    let mut budget = MAX_DESCENDANTS;
    collect_descendant_urls(session, item_root, 1, &mut budget, &mut paths);
    let root_depth = item_chain.len() - 1;
    match paths.len() {
        0 => Resolution::Unsupported(format!(
            "no file URL on item root (depth {root_depth}) or its descendants; container {container:?}"
        )),
        1 => Resolution::Resolved {
            path: paths.remove(0),
            source: format!("descendant AXURL of item root at depth {root_depth}"),
        },
        _ => Resolution::Ambiguous(paths),
    }
}

/// Depth-first search below `element`, bounded by `MAX_DESCENT` levels and
/// `budget` elements. Collects distinct file URLs.
pub(crate) fn collect_descendant_urls(
    session: u64,
    element: &AXUIElement,
    level: usize,
    budget: &mut usize,
    paths: &mut Vec<PathBuf>,
) {
    for child in children(element) {
        if *budget == 0 {
            info!(session, "descendant budget exhausted");
            return;
        }
        *budget -= 1;
        let role = string_attribute(&child, "AXRole");
        log_element(session, "down", level, &child, role.as_deref(), true);
        if let Some(path) = file_url_attribute(&child, "AXURL")
            && !paths.contains(&path)
        {
            paths.push(path);
        }
        if level < MAX_DESCENT {
            collect_descendant_urls(session, &child, level + 1, budget, paths);
        }
    }
}

pub(crate) fn log_element(
    session: u64,
    direction: &str,
    depth: usize,
    element: &AXUIElement,
    role: Option<&str>,
    with_names: bool,
) {
    let attributes: Vec<String> = DUMP_ATTRIBUTES
        .iter()
        .filter_map(|name| match attribute(element, name) {
            Ok(Some(value)) => Some(format!("{name}={}", Describe(&value))),
            Ok(None) => None,
            Err(err) => Some(format!("{name}=<{err:?}>")),
        })
        .collect();
    let names = if with_names {
        attribute_names(element).join(",")
    } else {
        String::new()
    };
    info!(
        session,
        direction,
        depth,
        role = ?role,
        attrs = %attributes.join(" "),
        names = %names,
        "ax element"
    );
}

fn element_at(
    system: &AXUIElement,
    (x, y): (f64, f64),
) -> Result<CFRetained<AXUIElement>, AXError> {
    let mut out: *const AXUIElement = ptr::null();
    // SAFETY: `out` is a valid place for the returned element.
    let err =
        unsafe { system.copy_element_at_position(x as f32, y as f32, NonNull::from(&mut out)) };
    if err != AXError::Success {
        return Err(err);
    }
    let element = NonNull::new(out.cast_mut()).ok_or(AXError::Failure)?;
    // SAFETY: the Copy rule gives us a +1 reference to a valid element.
    Ok(unsafe { CFRetained::from_raw(element) })
}

fn element_pid(element: &AXUIElement) -> Option<i32> {
    let mut pid: i32 = 0;
    // SAFETY: `pid` is a valid place for the result.
    let err = unsafe { element.pid(NonNull::from(&mut pid)) };
    (err == AXError::Success).then_some(pid)
}

/// Returns `Ok(None)` when the element has no value for `name`.
pub(crate) fn attribute(
    element: &AXUIElement,
    name: &str,
) -> Result<Option<CFRetained<CFType>>, AXError> {
    let name = CFString::from_str(name);
    let mut out: *const CFType = ptr::null();
    // SAFETY: `out` is a valid place for the returned value.
    let err = unsafe { element.copy_attribute_value(&name, NonNull::from(&mut out)) };
    if err == AXError::NoValue || err == AXError::AttributeUnsupported {
        return Ok(None);
    }
    if err != AXError::Success {
        return Err(err);
    }
    // SAFETY: the Copy rule gives us a +1 reference (or null).
    Ok(NonNull::new(out.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) }))
}

pub(crate) fn string_attribute(element: &AXUIElement, name: &str) -> Option<String> {
    let value = attribute(element, name).ok()??;
    value.downcast_ref::<CFString>().map(|s| s.to_string())
}

pub(crate) fn file_url_attribute(element: &AXUIElement, name: &str) -> Option<PathBuf> {
    let value = attribute(element, name).ok()??;
    value.downcast_ref::<CFURL>()?.to_file_path()
}

fn children(element: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
    element_array(element, "AXChildren")
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Reads an attribute that holds an array of elements, such as `AXChildren`
/// or `AXSelectedRows`. `Ok(None)` when the element has no such value.
pub(crate) fn element_array(
    element: &AXUIElement,
    name: &str,
) -> Result<Option<Vec<CFRetained<AXUIElement>>>, AXError> {
    let Some(value) = attribute(element, name)? else {
        return Ok(None);
    };
    if value.downcast_ref::<CFArray>().is_none() {
        return Err(AXError::IllegalArgument);
    }
    // SAFETY: checked to be a CFArray above; these attributes hold AXUIElements.
    let array = unsafe { CFRetained::cast_unchecked::<CFArray<AXUIElement>>(value) };
    Ok(Some(array.to_vec()))
}

fn attribute_names(element: &AXUIElement) -> Vec<String> {
    let mut out: *const CFArray = ptr::null();
    // SAFETY: `out` is a valid place for the returned array.
    let err = unsafe { element.copy_attribute_names(NonNull::from(&mut out)) };
    if err != AXError::Success {
        return vec![format!("<{err:?}>")];
    }
    let Some(array) = NonNull::new(out.cast_mut()) else {
        return Vec::new();
    };
    // SAFETY: the Copy rule gives us a +1 reference.
    let array: CFRetained<CFArray> = unsafe { CFRetained::from_raw(array) };
    // SAFETY: attribute names are CFStrings.
    let array = unsafe { CFRetained::cast_unchecked::<CFArray<CFString>>(array) };
    array.iter().map(|name| name.to_string()).collect()
}

pub(crate) fn parent(element: &AXUIElement) -> Option<CFRetained<AXUIElement>> {
    let value = attribute(element, "AXParent").ok()??;
    // SAFETY: AXParent always holds an AXUIElement.
    Some(unsafe { CFRetained::cast_unchecked::<AXUIElement>(value) })
}

/// Short human-readable form of an attribute value for the log.
struct Describe<'a>(&'a CFType);

impl fmt::Display for Describe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.0.downcast_ref::<CFString>() {
            return write!(f, "{:?}", s.to_string());
        }
        if let Some(url) = self.0.downcast_ref::<CFURL>() {
            return match url.to_file_path() {
                Some(path) => write!(f, "file:{}", path.display()),
                None => write!(f, "url:{url:?}"),
            };
        }
        if let Some(b) = self.0.downcast_ref::<CFBoolean>() {
            return write!(f, "{}", b.as_bool());
        }
        let text = format!("{:?}", self.0);
        let short: String = text.chars().take(80).collect();
        write!(f, "{{{}}}", short.replace('\n', " "))
    }
}
