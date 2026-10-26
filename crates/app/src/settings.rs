//! App settings as a GPUI global. In memory only: there is no storage layer
//! yet, so a change lasts until the app quits.

use gpui::{App, Global};

pub struct AppSettings(pub model::Settings);

impl Global for AppSettings {}

/// Startup values. `CHAOS_SEARCH_IN_PANE=1` turns the search-in-pane
/// experiment on (it is off by default).
pub fn init(cx: &mut App) {
    let search_in_pane = std::env::var("CHAOS_SEARCH_IN_PANE").is_ok_and(|v| v == "1");
    cx.set_global(AppSettings(model::Settings { search_in_pane }));
}

pub fn get(cx: &App) -> &model::Settings {
    &cx.global::<AppSettings>().0
}

pub fn update(cx: &mut App, f: impl FnOnce(&mut model::Settings)) {
    f(&mut cx.global_mut::<AppSettings>().0);
}
