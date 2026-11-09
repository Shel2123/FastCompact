//! AppKit user interface.

pub mod delegate;
pub mod logging;
#[expect(
    dead_code,
    reason = "used by the radial view, added in the next steps of stage 5"
)]
mod palette;
pub mod parameters;
pub mod radial_panel;
pub mod radial_view;
pub mod status_item;
#[expect(
    dead_code,
    reason = "used by the radial view, added in the next steps of stage 5"
)]
mod symbols;
