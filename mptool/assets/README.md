# Screen assets

Built into the program for the little screen (`src/screen.rs`).

| File | What | Made with |
|---|---|---|
| `logo-180.rgba` | The white Minipass logo, 180 px wide, raw RGBA | `magick -background none -density 300 minipass-logo-white.svg -resize 180x -depth 8 rgba:logo-180.rgba` (from minipass_env/Marketing/brand/logo-spec) |
| `fonts/LiberationSans-*.ttf` | The letters (SIL Open Font License, `fonts/LICENSE-liberation.txt`) | Copied from Arch's `ttf-liberation` |
| `fonts/SymbolsNerdFont.ttf` | The icons: only the 9 used | `pyftsubset SymbolsNerdFont-Regular.ttf --unicodes=U+F05E0,U+F0159,U+F0028,U+F01BC,U+F059F,U+F05A9,U+F0200,U+F02CA,U+F0222` (Nerd Fonts, `fonts/LICENSE-nerd-fonts-symbols.txt`) |
