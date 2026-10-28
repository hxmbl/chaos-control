//! Phosphor UI icons (regular weight only) and brand marks, drawn as
//! single-colour SVG masks. Sizes come from `tokens::icon`.

use gpui::{Hsla, IntoElement, Pixels, Styled, Svg, svg};
use model::Brand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconName {
    Archive,
    ArrowUp,
    CaretDown,
    CaretRight,
    CaretUp,
    ChatCircle,
    CopySimple,
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
    BrandDevin,
    BrandCopilot,
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
    pub const ALL: [IconName; 27] = [
        IconName::Archive,
        IconName::ArrowUp,
        IconName::CaretDown,
        IconName::CaretRight,
        IconName::CaretUp,
        IconName::ChatCircle,
        IconName::CopySimple,
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
        IconName::BrandDevin,
        IconName::BrandCopilot,
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
            IconName::Archive => "icons/phosphor/archive.svg",
            IconName::ArrowUp => "icons/phosphor/arrow-up.svg",
            IconName::CaretDown => "icons/phosphor/caret-down.svg",
            IconName::CaretRight => "icons/phosphor/caret-right.svg",
            IconName::CaretUp => "icons/phosphor/caret-up.svg",
            IconName::ChatCircle => "icons/phosphor/chat-circle.svg",
            IconName::CopySimple => "icons/phosphor/copy-simple.svg",
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
            IconName::BrandDevin => "icons/brands/devin.svg",
            IconName::BrandCopilot => "icons/brands/github-copilot.svg",
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
    }
}

/// A square icon of `size`, tinted `color`. GPUI caches the rasterised mask per size.
pub fn icon(name: IconName, size: impl Into<Pixels>, color: Hsla) -> impl IntoElement + Styled {
    let size = size.into();
    let el: Svg = svg()
        .path(name.path())
        .size(size)
        .flex_none()
        .text_color(color);
    el
}
