# Bundled assets

Everything here is compiled into the binary by `crates/ui/src/assets.rs`.

| Path | What | Licence |
| --- | --- | --- |
| `fonts/PlusJakartaSans-*.ttf` | [Plus Jakarta Sans](https://github.com/tokotype/PlusJakartaSans) Regular, Medium, SemiBold, Bold | SIL Open Font License 1.1, see `fonts/OFL.txt` |
| `icons/phosphor/*.svg` | [Phosphor Icons](https://github.com/phosphor-icons/core), only the ones the UI uses | MIT, see `icons/phosphor/LICENSE` |
| `icons/brands/*.svg` | Brand marks via [Iconify](https://iconify.design): [Simple Icons](https://github.com/simple-icons/simple-icons) and [SVG Logos](https://github.com/gilbarbara/logos) (Devin) | CC0-1.0, see `icons/brands/NOTICE.md` |

To add a Phosphor icon, copy its SVG from `phosphor-icons/core/assets/<weight>/`,
then add it to `FILES` in `assets.rs` and to `IconName` in `icon.rs`. The
`every_icon_is_bundled` test catches a missing entry.
