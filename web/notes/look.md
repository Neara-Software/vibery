# Fibery look & feel (neara.fibery.io, measured 1440x900, light theme)
Screenshots in this dir: home.png (Snippet page + sidebar), note.png (Note top), note-bottom.png (relations/backlinks), search.png (search overlay). Computed from the live DOM + CDN CSS vars.

## Fonts
- UI: `"Inter Variable","SF Pro Display",-apple-system,"system-ui",sans-serif` (use Inter if available). Base 14px/21px, weight **410** (variable font; 400 is fine), headings 600.
- Mono (inline code): `SFMono-Regular,Menlo,Consolas,"PT Mono","Liberation Mono",monospace` 13.6px/20.4px.

## Colours
- App canvas / sidebar bg `#f5f5f5`; panel (main card) bg `#fff`; subtle panel content bg `#f7f7f7`.
- Text `#171717` (body); sidebar items `rgba(23,23,23,.9)`; secondary `rgba(23,23,23,.7)`; labels/fields `#5e5e5e`; muted/placeholder/breadcrumbs `#9e9e9e`; subtle `#787878`.
- **Accent = orange `#bf5b0f`** (primary button, "To create ..." line, checked checkbox, selected tint `#f7e0d3`, `rgba(191,91,15,.1)` subtle). Links `#3869cc` with underline `0.5px solid #3869cc66`.
- Separator `rgba(0,0,0,.1)`; popup border `#c7c7c773`; hover bg `#e8e8e8` (menu items), `#f5f5f5` (list hover).
- Count badge (Inbox 99+): bg `#78787833`, 11px, radius 8, pad 0 6, h16.
- Inline code: text `#d33e4e`, bg `#f5f5f5`, radius 4, pad 3px 4px.
- Info box `#fff8b980`; banners: neutral `#f5f5f5`, accent `#fdf3ee`, warning `#fffbcf`, success `#eff7f5`, error `#fff1f1`.
- Highlights: yellow `#ffecc4`, blue `#e1eeff`, green `#daf6ca`, red `#ffe8e6`, grey `#eee`.
- Blockquote: 3px solid `#5e5e5e` left bar.
- Done/checked todo text greyed (~`#8a8a8a`); checked box filled `#bf5b0f` white tick; unchecked 15px box, 1.5px border `#8a8a8a`, radius 4.

## Layout shell
- Whole page is `#f5f5f5`. **Sidebar width 284px**, no border, same bg as canvas.
- Main panel is a floating white card: margin 8px (6px left of sidebar), **radius 12px**, shadow `0 0 0 1px rgba(199,199,199,.45), 0 4px 8px rgba(0,0,0,.09)`. At 1440: x=290..1432, y=8..892.
- Panel header: **40px** high, white, bottom hairline `rgba(0,0,0,.08)`. Row 1: left = database icon + space name ("Shared", 13px `#5e5e5e`) ; right = icons `»` expand, open-in-panel, close X (16px, `#5e5e5e`). Row 2 (y≈48, ~28px): type badge, link icon, `#27735` id (13px `#787878`, `#` smaller), star outline (filled `#f5a623` if favorited); right: "Share" (users icon + 12px text), comment, history, `...`.
- When scrolled past the title, header row 2 shows the entity title (16px/600, with its small type icon) in place of the type badge.
- Content column: centered, ~1046px wide (x=322..1368 at 1440), inner text 998px wide at x=362. Padding ~40px left of editor text.

## Sidebar (top to bottom, x padding 12)
1. Workspace row y≈8..38: Neara "N" red logo (18px) + "Neara" 16px/600 + chevron; right: sidebar-collapse icon, orange 24px square avatar (`#bf5b0f`, white glyph, radius 6), outlined new-entity icon button (white, 1px border, radius 6).
2. Nav items (each 28px high, icon 18px at x=16, label at x=42, 14px/410): Home, Fibery AI (sparkle), Search (magnifier), Inbox (bell with orange dot; right-aligned count badge "99+" at x=233).
3. Segmented control y≈179: "Workspace" (active: white bg, radius 5, 12px/500, shadow subtle) | "Personal" on track `rgba(232,232,232,.8)` (h28, radius 6, pad 2px; tabs h24 radius 5 pad 4px 8px 12px/500 `#5e5e5e`); right: search icon, collapse-all icon.
4. Tree: rows 28px (folder icon 18px, label 14px, tiny caret `▸` at x=18 in `#9e9e9e`); folders: General, Deployment, Development, Commercial, Operations, Other. Children indent 14-16px. Hover bg `#e8e8e8` radius 6; selected = same with accent-tint.
5. Bottom bar (y≈870): "? Help" 14px `rgba(23,23,23,.7)`; right: history + trash icons 16px.

## Entity page (Note / Snippet etc.)
- Type badge: uppercase 11px/600 white on solid colour, radius 4, pad 1px 4px, h16 (border 1px same colour). Colours: NOTE `#f09520`, SNIPPET `#9a3fab` (purple), Scope green `#4aa564`, Issue red `#cc3d33`, Bet violet `#5b44a8`, Solution `#7a2a8c`, Event `#d24a6e`. Same colours used for 18px rounded-square letter icons (N, S, !, B, E) next to titles, with white letter 11px.
- Title: `textarea` 30px/39px weight **600**, `#171717`, no border, x=350, y≈105 (≈50px below header), pad 6px. Whole title block max width ~1022.
- Field row (under title, y≈145): wrap of small chips, gap 6-8px, 12px text, h24: filled fields are chips h24 radius 6 pad 2px (inner text pad 0 4px) with 1px `#e0e0e0` outline (e.g. date "Sep 7, 2026", `2026-10-24 - 2026-10-30` with red date-range icon, avatar + "Tom Birch" + `>` arrow). Empty fields are ghost chips: `Add Parent note` in `#9e9e9e` with a 10px coloured relation-database icon on the left, light border `#ececec`, radius 6, h24 (text h20). They wrap onto a 2nd line (row gap 8). People: 14-16px round-ish avatar (radius 4 in lists; initials like "TB" on salmon `#f4a38c`/pastel bg, 8px font) + name.
- Then a thin hr `rgba(0,0,0,.1)` (when the field list is collapsed there is a small up/down chevron toggle at the left).
- Doc section label: "Description" 14px/410 `#5e5e5e` with small lines icon, pad 2px 4px, at x=378 (≈24px high), then 24px gap then rich text. A small `</>` toggle button (1px border, radius 4, 24px) sits at the far right x≈1340.
- Rich text: body 16px/24px `#171717`, p pad 3px 0; ul margin 3px 3px 3px 12px, li margin-left 12px, bullets disc/circle (nested), no extra spacing; h1 28px/36.4 weight 600 margin 3px 0 8px; **h2 24px/31.2 weight 600 margin 24px 0 4px**; h3 ~20px/600; hr margin 8px 0; todos: custom checkbox 15px at left, text indent ~24px, nested todos indent 28px; entity mentions inline: small 16px coloured letter icon + title with underline `1px solid #171717` at 0.2 alpha; raw URLs `#3869cc`.
- Relation sections (below doc): per-relation block: label row (icon + name, 14px `#5e5e5e`), then a box (bg `#fcfcfc`, 1px `#e6e6e6`, radius 12 outer pad 8px, full content width 1022; inner rows pad 6px, `No Items` row h28): empty -> "No Items" 12px `#787878`; then a row "+ Link or create" 12px `#5e5e5e`. Non-empty: rows with 18px letter icon + title 14-16px, 24px per row.
- **"Referenced N times"** section (backlinks): label like above with a lines-icon, then one box; per referencing entity a header row (letter icon + title 16px, right side "2 months ago" 12px `#787878` + 18px avatar) followed by an inset white card (1px border `#e6e6e6`, radius 8, pad 8px 12px, 16px text) that shows the paragraph containing the mention with the mention rendered as icon+underlined title, author mentions as blue `#3869cc` text with avatar chip. This replaces a classic comment feed; comments (when present) appear in a right-hand comments panel (bubble `#fff`, highlight `#f7e78799` on commented text) toggled by the comment icon in header.

## Search overlay (Search nav item; full panel replaces the entity panel, same card)
- Top: 48px row, magnifier at x=258, input 16px (no border), clear icon at right (X in circle), hairline below.
- Filter row (y≈63): ghost buttons 12px/500 `#5e5e5e`: "Anything v" (database icon), "Date v", "Created by v", "People v", "Unfinished only"; sort icon far right.
- Create hint (orange `#bf5b0f` 13px): "+ To create rvo choose Database or View" + outlined orange select pill "Anything v ↵".
- Results list: each row ~48px (title line 14px/21 `#171717`, with the matched term **bold/darker** (rest also dark); second line 12px `#9e9e9e`: `Shared > Notes` with small chevron). Left: 14px letter-icon (colour = database). Right: `#31188` pill: 11px `#5e5e5e`, bg `#78787833`, radius 4, pad 1px 4px. Row padding 8px 16px, radius 8, selected/hover bg `#ececec` (seen ~`#eee`).
- Footer bar (bottom, ~32px, hairline above, 12px `#787878`): `↑↓ navigate  ↵ open  ⌘↵ open in a new tab  ⌥↵ open in a new panel` with keycap pills (1px `#c7c7c780`, radius 4, `#9e9e9e`).

## Misc
- Icon style: 16-18px stroke line icons, 1.5px, colour `#5e5e5e`. Hover on icon buttons: bg `#e8e8e8`, radius 6.
- Transitions are instant-feeling (<100ms); no heavy shadows except the main card.
- Scrollbars hidden/overlay thin.
- Caveats: coloured letter-icon hexes for non-Note databases are eyeballed from screenshots; everything else measured.
