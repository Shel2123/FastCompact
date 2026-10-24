//! Resolver thread: system-wide AX hit-test at a fixed point, then an
//! attribute walk from the hit element towards the window.
//!
//! `AXUIElement` is not `Send`, so both phases run on this thread and only
//! plain Rust values are sent back.

use std::fmt;
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFBoolean, CFRetained, CFString, CFType, CFURL};
use tracing::{error, info};

/// Request from the main thread. `point` is in Quartz global coordinates and
/// never changes after the hotkey press.
pub struct Request {
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
    Resolved { path: PathBuf, depth: usize },
    Unsupported(String),
    PermissionDenied,
}

/// Walk at most this many levels above the hit element.
const MAX_DEPTH: usize = 12;
const MESSAGING_TIMEOUT_SECS: f32 = 0.5;

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

pub fn spawn(sink: impl Fn(Event) + Send + 'static) -> mpsc::Sender<Request> {
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
                resolve(&system, &request, &sink);
            }
        });
    if let Err(err) = spawned {
        error!(%err, "resolver thread not started");
    }
    sender
}

fn resolve(system: &AXUIElement, request: &Request, sink: &impl Fn(Event)) {
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

/// Logs the chain from the hit element upwards and picks the first own file
/// URL found below any container.
fn walk(session: u64, hit: CFRetained<AXUIElement>) -> Resolution {
    let mut current = Some(hit);
    let mut found = None;
    let mut stop = None;
    for depth in 0..MAX_DEPTH {
        let Some(element) = current.take() else {
            break;
        };
        let role = match attribute(&element, "AXRole") {
            Err(AXError::APIDisabled) => return Resolution::PermissionDenied,
            Ok(value) => value.and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string())),
            Err(_) => None,
        };
        let attributes: Vec<String> = DUMP_ATTRIBUTES
            .iter()
            .filter_map(|name| match attribute(&element, name) {
                Ok(Some(value)) => Some(format!("{name}={}", Describe(&value))),
                Ok(None) => None,
                Err(err) => Some(format!("{name}=<{err:?}>")),
            })
            .collect();
        info!(session, depth, role = ?role, attrs = %attributes.join(" "), "ax element");

        if found.is_none() && stop.is_none() {
            if let Some(path) = file_url_attribute(&element, "AXURL") {
                found = Some(Resolution::Resolved { path, depth });
            } else if let Some(role) = role.as_deref().filter(|r| CONTAINER_ROLES.contains(r)) {
                stop = Some(format!(
                    "reached container {role} at depth {depth} without a file URL"
                ));
            }
        }
        if role.as_deref() == Some("AXApplication") {
            break;
        }
        current = parent(&element);
    }
    found.unwrap_or_else(|| {
        Resolution::Unsupported(stop.unwrap_or_else(|| "no file URL in the chain".to_owned()))
    })
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
fn attribute(element: &AXUIElement, name: &str) -> Result<Option<CFRetained<CFType>>, AXError> {
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

fn string_attribute(element: &AXUIElement, name: &str) -> Option<String> {
    let value = attribute(element, name).ok()??;
    value.downcast_ref::<CFString>().map(|s| s.to_string())
}

fn file_url_attribute(element: &AXUIElement, name: &str) -> Option<PathBuf> {
    let value = attribute(element, name).ok()??;
    value.downcast_ref::<CFURL>()?.to_file_path()
}

fn parent(element: &AXUIElement) -> Option<CFRetained<AXUIElement>> {
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
