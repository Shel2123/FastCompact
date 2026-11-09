//! AppKit user interface.

pub mod delegate;
pub mod logging;
#[expect(
    dead_code,
    reason = "used by the radial view, added in the next steps of stage 5"
)]
mod menu_session;
#[expect(
    dead_code,
    reason = "used by the radial view, added in the next steps of stage 5"
)]
mod palette;
pub mod parameters;
pub mod radial_panel;
#[expect(
    dead_code,
    reason = "used by the radial panel, added in the next step of stage 5"
)]
mod radial_view;
pub mod status_item;
// Tests of the view use all of it, so only the normal build has dead code.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by the radial panel, added in the next step of stage 5"
    )
)]
mod symbols;
