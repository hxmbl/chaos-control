//! Theme, bundled assets and the small set of components the shell is built from.

mod assets;
mod editing;
mod glass;
mod icon;
pub mod pane;
mod pill;
mod text;
pub mod text_input;
mod theme;
pub mod tokens;
mod tooltip;

pub use assets::{Assets, load_fonts};
pub use glass::Glass;
pub use icon::{IconName, brand_icon, icon};
pub use pane::{Pane, PaneState, QuickAction, switch};
pub use pill::pill;
pub use text::{Ellipsis, Truncated, truncated};
pub use text_input::{TextInput, TextInputEvent};
pub use theme::{ActiveTheme, Appearance, FONT_FAMILY, Theme};
pub use tokens::{Elevation, TypeStyle, TypeStyleExt};
pub use tooltip::Tooltip;

/// Registers globals and key bindings the components rely on. Call once at startup.
pub fn init(cx: &mut gpui::App) {
    theme::init(cx);
    text_input::bind_keys(cx);
    pane::bind_keys(cx);
}
