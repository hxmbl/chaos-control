//! Phosphor UI icons (regular weight only) and brand marks, drawn as
//! single-colour SVG masks. Sizes come from `tokens::icon`.

use gpui::{Hsla, Pixels, Styled, Svg, svg};
use model::Brand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconName {
    /// The Chaos Control mark (single colour, like the rest).
    AppMark,
    Archive,
    ArrowUp,
    ArrowClockwise,
    CaretDown,
    CaretRight,
    CaretUp,
    ChatCircle,
    Check,
    CircleDashed,
    CopySimple,
    DownloadSimple,
    GearSix,
    Hash,
    MagnifyingGlass,
    Minus,
    Plus,
    SidebarLeft,
    SidebarRight,
    Square,
    X,
    BrandClaude,
    BrandCursor,
    BrandDevin,
    BrandCopilot,
    BrandGemini,
    BrandGitHub,
    BrandLinear,
    BrandMistral,
    BrandOpenAi,
    BrandOpencode,
    BrandPi,
    BrandVercel,
    BrandZed,
}

impl IconName {
    pub const ALL: [IconName; 34] = [
        IconName::AppMark,
        IconName::Archive,
        IconName::ArrowUp,
        IconName::ArrowClockwise,
        IconName::CaretDown,
        IconName::CaretRight,
        IconName::CaretUp,
        IconName::ChatCircle,
        IconName::Check,
        IconName::CircleDashed,
        IconName::CopySimple,
        IconName::DownloadSimple,
        IconName::GearSix,
        IconName::Hash,
        IconName::MagnifyingGlass,
        IconName::Minus,
        IconName::Plus,
        IconName::SidebarLeft,
        IconName::SidebarRight,
        IconName::Square,
        IconName::X,
        IconName::BrandClaude,
        IconName::BrandCursor,
        IconName::BrandDevin,
        IconName::BrandCopilot,
        IconName::BrandGemini,
        IconName::BrandGitHub,
        IconName::BrandLinear,
        IconName::BrandMistral,
        IconName::BrandOpenAi,
        IconName::BrandOpencode,
        IconName::BrandPi,
        IconName::BrandVercel,
        IconName::BrandZed,
    ];

    pub fn path(self) -> &'static str {
        match self {
            IconName::AppMark => "icons/chaos-control.svg",
            IconName::Archive => "icons/phosphor/archive.svg",
            IconName::ArrowUp => "icons/phosphor/arrow-up.svg",
            IconName::ArrowClockwise => "icons/phosphor/arrow-clockwise.svg",
            IconName::CaretDown => "icons/phosphor/caret-down.svg",
            IconName::CaretRight => "icons/phosphor/caret-right.svg",
            IconName::CaretUp => "icons/phosphor/caret-up.svg",
            IconName::ChatCircle => "icons/phosphor/chat-circle.svg",
            IconName::Check => "icons/phosphor/check.svg",
            IconName::CircleDashed => "icons/phosphor/circle-dashed.svg",
            IconName::CopySimple => "icons/phosphor/copy-simple.svg",
            IconName::DownloadSimple => "icons/phosphor/download-simple.svg",
            IconName::GearSix => "icons/phosphor/gear-six.svg",
            IconName::Hash => "icons/phosphor/hash.svg",
            IconName::MagnifyingGlass => "icons/phosphor/magnifying-glass.svg",
            IconName::Minus => "icons/phosphor/minus.svg",
            IconName::Plus => "icons/phosphor/plus.svg",
            IconName::SidebarLeft => "icons/phosphor/sidebar-simple.svg",
            // Phosphor's sidebar-simple, mirrored.
            IconName::SidebarRight => "icons/phosphor/sidebar-simple-right.svg",
            IconName::Square => "icons/phosphor/square.svg",
            IconName::X => "icons/phosphor/x.svg",
            IconName::BrandClaude => "icons/brands/claude.svg",
            IconName::BrandCursor => "icons/brands/cursor.svg",
            IconName::BrandDevin => "icons/brands/devin.svg",
            IconName::BrandCopilot => "icons/brands/github-copilot.svg",
            IconName::BrandGemini => "icons/brands/gemini.svg",
            IconName::BrandGitHub => "icons/brands/github.svg",
            IconName::BrandLinear => "icons/brands/linear.svg",
            IconName::BrandMistral => "icons/brands/mistral.svg",
            IconName::BrandOpenAi => "icons/brands/openai.svg",
            IconName::BrandOpencode => "icons/brands/opencode.svg",
            IconName::BrandPi => "icons/brands/pi.svg",
            IconName::BrandVercel => "icons/brands/vercel.svg",
            IconName::BrandZed => "icons/brands/zed.svg",
        }
    }
}

pub fn brand_icon(brand: Brand) -> IconName {
    match brand {
        Brand::GitHub => IconName::BrandGitHub,
        Brand::Linear => IconName::BrandLinear,
        Brand::Zed => IconName::BrandZed,
        Brand::Vercel => IconName::BrandVercel,
        Brand::OpenAi => IconName::BrandOpenAi,
        Brand::Claude => IconName::BrandClaude,
        Brand::Devin => IconName::BrandDevin,
        Brand::Copilot => IconName::BrandCopilot,
        Brand::Mistral => IconName::BrandMistral,
        Brand::Opencode => IconName::BrandOpencode,
        Brand::Pi => IconName::BrandPi,
        Brand::Gemini => IconName::BrandGemini,
        Brand::Cursor => IconName::BrandCursor,
    }
}

/// A square icon of `size`, tinted `color`. GPUI caches the rasterised mask per size.
/// Returns [`Svg`] so callers can chain on `with_transformation` (rotation, scale).
pub fn icon(name: IconName, size: impl Into<Pixels>, color: Hsla) -> Svg {
    let size = size.into();
    svg()
        .path(name.path())
        .size(size)
        .flex_none()
        .text_color(color)
}
