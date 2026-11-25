//! Accessibility elements of Finder.
//!
//! `AXUIElement` is not `Send`, so the resolver creates and uses these on
//! its own thread only.

use std::ptr::{self, NonNull};

use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType, CFURL};
use tracing::warn;

use super::area::{AxError, AxNode, Url};

/// How long Finder may take to answer one request, in seconds.
const MESSAGING_TIMEOUT_SECS: f32 = 0.5;

/// An accessibility element.
pub(crate) struct AxElement(CFRetained<AXUIElement>);

impl AxElement {
    /// The application with `pid`, answering each request within
    /// [`MESSAGING_TIMEOUT_SECS`].
    pub(crate) fn application(pid: i32) -> Self {
        // SAFETY: any pid is accepted; for a pid without an application the
        // requests fail.
        let app = unsafe { AXUIElement::new_application(pid) };
        // SAFETY: `app` is a valid element.
        let err = unsafe { app.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
        if err != AXError::Success {
            warn!(?err, "accessibility timeout not set");
        }
        Self(app)
    }

    fn attribute(&self, name: &str) -> Result<Option<CFRetained<CFType>>, AxError> {
        let name = CFString::from_str(name);
        let mut out: *const CFType = ptr::null();
        // SAFETY: `out` is a valid place for the returned value.
        let err = unsafe { self.0.copy_attribute_value(&name, NonNull::from(&mut out)) };
        if err == AXError::NoValue || err == AXError::AttributeUnsupported {
            return Ok(None);
        }
        if err != AXError::Success {
            return Err(ax_error(err));
        }
        // SAFETY: the Copy rule gives a +1 reference, or null.
        Ok(NonNull::new(out.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) }))
    }
}

fn ax_error(err: AXError) -> AxError {
    match err {
        AXError::APIDisabled => AxError::Disabled,
        AXError::CannotComplete => AxError::CannotComplete,
        other => AxError::Other(other.0),
    }
}

impl AxNode for AxElement {
    fn string(&self, name: &str) -> Result<Option<String>, AxError> {
        Ok(self
            .attribute(name)?
            .and_then(|value| value.downcast_ref::<CFString>().map(ToString::to_string)))
    }

    fn element(&self, name: &str) -> Result<Option<Self>, AxError> {
        Ok(self
            .attribute(name)?
            .and_then(|value| value.downcast::<AXUIElement>().ok())
            .map(Self))
    }

    fn elements(&self, name: &str) -> Result<Option<Vec<Self>>, AxError> {
        let Some(value) = self.attribute(name)? else {
            return Ok(None);
        };
        let Ok(array) = value.downcast::<CFArray>() else {
            return Err(AxError::Other(AXError::IllegalArgument.0));
        };
        // SAFETY: the element arrays of the accessibility API, such as
        // `AXChildren` and `AXSelectedRows`, hold `AXUIElement`s.
        let array = unsafe { CFRetained::cast_unchecked::<CFArray<AXUIElement>>(array) };
        Ok(Some(array.to_vec().into_iter().map(Self).collect()))
    }

    fn url(&self, name: &str) -> Result<Option<Url>, AxError> {
        let Some(value) = self.attribute(name)? else {
            return Ok(None);
        };
        let Ok(url) = value.downcast::<CFURL>() else {
            return Ok(Some(Url::Other));
        };
        let is_file = url
            .scheme()
            .is_some_and(|scheme| scheme.to_string().eq_ignore_ascii_case("file"));
        Ok(Some(match url.to_file_path() {
            Some(path) if is_file => Url::File(path),
            _ => Url::Other,
        }))
    }
}
