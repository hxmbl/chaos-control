//! Everything the app ships with is compiled into the binary: no files to find at runtime.

use std::borrow::Cow;

use gpui::{App, AssetSource, Result, SharedString};

macro_rules! embed {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../../../assets/", $path)) as &[u8])),*]
    };
}

/// (path, bytes) for every bundled asset. Paths are what `svg().path(..)` takes.
static FILES: &[(&str, &[u8])] = embed![
    "icons/phosphor/archive.svg",
    "icons/phosphor/arrow-up.svg",
    "icons/phosphor/caret-down.svg",
    "icons/phosphor/caret-right.svg",
    "icons/phosphor/caret-up.svg",
    "icons/phosphor/chat-circle.svg",
    "icons/phosphor/copy-simple.svg",
    "icons/phosphor/gear-six.svg",
    "icons/phosphor/hash.svg",
    "icons/phosphor/magnifying-glass.svg",
    "icons/phosphor/minus.svg",
    "icons/phosphor/plus.svg",
    "icons/phosphor/sidebar-simple-right.svg",
    "icons/phosphor/sidebar-simple.svg",
    "icons/phosphor/square.svg",
    "icons/phosphor/x.svg",
    "icons/brands/claude.svg",
    "icons/brands/cursor.svg",
    "icons/brands/devin.svg",
    "icons/brands/gemini.svg",
    "icons/brands/github-copilot.svg",
    "icons/brands/github.svg",
    "icons/brands/linear.svg",
    "icons/brands/mistral.svg",
    "icons/brands/openai.svg",
    "icons/brands/opencode.svg",
    "icons/brands/pi.svg",
    "icons/brands/vercel.svg",
    "icons/brands/zed.svg",
];

static FONTS: &[&[u8]] = &[
    include_bytes!("../../../assets/fonts/PlusJakartaSans-Regular.ttf"),
    include_bytes!("../../../assets/fonts/PlusJakartaSans-Medium.ttf"),
    include_bytes!("../../../assets/fonts/PlusJakartaSans-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/PlusJakartaSans-Bold.ttf"),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(FILES
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::new_static(p))
            .collect())
    }
}

/// Registers the bundled Plus Jakarta Sans faces with GPUI's text system.
pub fn load_fonts(cx: &App) -> Result<()> {
    cx.text_system()
        .add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IconName;

    #[test]
    fn every_icon_is_bundled() {
        for icon in IconName::ALL {
            assert!(
                Assets.load(icon.path()).unwrap().is_some(),
                "{} is missing from FILES",
                icon.path()
            );
        }
    }
}
