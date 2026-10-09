//! Brand artwork shared by the chat frontends.

use leptos::prelude::*;

/// Solid pink used for the crab body, legs and claws (the midpoint of the
/// favicon's radial gradient).
const CRAB_PINK: &str = "#FF3A92";

/// One (left) half of the crab: claw, eye and legs. The right half is the same
/// artwork mirrored, so this is rendered twice instead of being referenced via
/// `<use href="#id">`.
#[component]
fn CrabSide() -> impl IntoView {
    view! {
        <path d="M69.2 74.4 C64.2 74.2 60.6 76.4 59.2 79.4 C57.6 82.6 56.8 85.4 57.2 87.8 C57.6 89.6 59.4 90.2 61.6 89.2 C64.4 87.8 66.8 85.6 68.8 83.6 C71.4 81 73.6 79.8 73.4 77.2 C73.2 74.8 71.4 74.2 69.2 74.4Z" />
        <ellipse cx="70" cy="75.2" rx="2.7" ry="4.1" fill="#081028" stroke="none" transform="rotate(-18 70 75.2)" />
        <path d="M58 89.2C59.6 92.6 62.8 95.2 66.6 97" fill="none" stroke-width="3.1" />
        <path d="M51 91.5C57 95.6 63.2 98.6 68.8 100.2" fill="none" stroke-width="5.2" />
        <path d="M50.5 108.8C57.5 107.8 63.5 107.2 69 106.8" fill="none" stroke-width="5.6" />
        <path d="M59 119.6C64 115.6 67.8 112.8 71.6 110.8" fill="none" stroke-width="4.8" />
        <path d="M72.2 116L71.4 126.4" fill="none" stroke-width="4.4" />
    }
}

/// The rust-bot crab badge, inlined so it can be sized with Tailwind classes
/// and never depends on asset paths (embed/SSO contexts).
///
/// Artwork is derived from `chat-ui/favicon.svg` — keep the two in sync. It
/// deliberately uses a solid fill and no element ids: the logo is mounted more
/// than once (desktop sidebar + mobile drawer), and id references
/// (`url(#gradient)`, `<use href>`) can resolve to a copy inside a
/// `display:none` subtree, which makes the crab vanish against its dark badge.
#[component]
pub fn BrandLogo(#[prop(into, default = "h-6 w-6".to_string())] class: String) -> impl IntoView {
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="13 30 127 127"
            role="img"
            aria-label="rust-bot"
            class=class
        >
            <circle cx="76.5" cy="93.5" r="63.5" fill="#081028" />
            <g transform="translate(76.5 93.5) scale(1.87) translate(-76.5 -100)">
                <g fill=CRAB_PINK stroke=CRAB_PINK stroke-linecap="round" stroke-linejoin="round">
                    <g>
                        <CrabSide />
                    </g>
                    <g transform="translate(156 0) scale(-1 1)">
                        <CrabSide />
                    </g>
                    <ellipse cx="78" cy="106" rx="20.2" ry="16.2" stroke="none" />
                </g>
            </g>
        </svg>
    }
}
