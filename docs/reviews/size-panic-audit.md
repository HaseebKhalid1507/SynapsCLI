# TUI Size‑Panic Audit — `dev` @ HEAD

Read‑only audit of the render path for panics on degenerate terminal sizes
(width/height of 0/1/2 or smaller than the code assumes). Triggered by the
recently‑fixed `min > max` in `draw.rs::toast_dims` (now `clamp(18u16.min(max_w), max_w)`).

Scope: `draw.rs`, `render.rs`, `viewport.rs`, `toast.rs`, `sidecar.rs`,
`markdown.rs`, `highlight.rs`, `lightbox.rs`, `plugins/draw.rs`,
`settings/draw.rs`, `models/{mod,input}.rs`, `agent-engine/src/help.rs`.

Methodology: grepped for every `clamp(…)`, every non‑saturating `-`, every
`/` and `%`, every layout‑slot index, every `unwrap()` on the render side,
and read the surrounding control flow for guards.

---

## TL;DR

**No remaining panic‑class bugs found that a user can reach by resizing.**
All `clamp(min, max)` sites either use constant `(min, max)` pairs with
`min ≤ max`, or guard the area‑derived `max` with `.max(min)` /
`min.min(max_w)`. All `len() - 1` and `total_rows - 1` underflows are
guarded by emptiness / monotone‑start invariants. All `Layout::split`
returns are indexed within the constraint count. Cosmetic clipping on
oversize popup rects (`MEDIUM`, listed below) is a render‑quality issue,
not a crash.

---

## RISKY findings

None reachable from a resize on `dev`. The class is dead pending the
`MEDIUM` cosmetic items below.

---

## MEDIUM — cosmetic, not panics (won't crash, will draw outside intended area)

ratatui clips out‑of‑buffer writes, so these are render artifacts / tmux
edge‑residue territory, not crashes. Listed for completeness.

### M1 `plugins/draw.rs:19` overlay min‑width can exceed area

```rust
area.width.saturating_sub(4).clamp(24, OVERLAY_MAX_WIDTH /* 70 */)
```

If `area.width < 24`, popup width is forced up to 24, then drawn from
`x = area.x + area.width.saturating_sub(24)/2 = area.x`. Rect extends past
the right edge of the parent. ratatui clips → no panic. Fix (for polish):
`.clamp(24u16.min(area.width.max(1)), OVERLAY_MAX_WIDTH.min(area.width).max(1))`.

### M2 `settings/draw.rs:453,504` popup forces width 40/20 in smaller areas

Same pattern as M1, same non‑panic outcome. Min `clamp(40,100)` /
`clamp(20,100)`. Fix: mirror the `area.height.saturating_sub(2).max(3)`
guard already used on the height axis (lines 455, 505) — those are the
**correct** pattern.

### M3 `draw.rs:1480` secret prompt modal: `area.width.min(62).max(30)`

```rust
let width  = area.width.min(62).max(30);
let height = 7u16;
```

`.min(62).max(30)` forces width ≥ 30 even if `area.width < 30`. Same with
hardcoded `height = 7` on a `≤ 6`‑row terminal. Rect extends past frame,
ratatui clips. Fix:

```rust
let width  = area.width.min(62).max(30u16.min(area.width).max(1));
let height = 7u16.min(area.height.max(1));
```

### M4 `plugins/draw.rs:391` `centered_overlay_with_height` clamps height but not width

```rust
let rect = Rect { x, y, width: w, height: h.min(area.height) };
```

`w` comes from `overlay_outer_width` (M1) and can exceed `area.width`. Same
non‑panic result. Fix: `width: w.min(area.width)`.

---

## SAFE — explicitly verified

### Clamps (every `.clamp(` site)

| Site | Expression | Why SAFE |
|---|---|---|
| `draw.rs:369` | `((width as usize).saturating_sub(42)).clamp(8, 28)` | const, min ≤ max |
| `draw.rs:1552` | `…clamp(18u16.min(max_w), max_w)` | guarded min (the fix) |
| `draw.rs:1554` | `…clamp(3u16.min(max_h), max_h)` | guarded min (the fix) |
| `highlight.rs:117‑119, 336‑338` | `.clamp(0, 255)` on `i16` channel math | constant range |
| `plugins/draw.rs:19` | `.clamp(24, 70)` | min ≤ max (cosmetic M1) |
| `settings/draw.rs:453` | `.clamp(40, 100)` | min ≤ max (cosmetic M2) |
| `settings/draw.rs:455` | `needed.clamp(3, area.height.saturating_sub(2).max(3))` | **correct pattern**: max guarded ≥ min |
| `settings/draw.rs:504` | `.clamp(20, 100)` | min ≤ max (cosmetic M2) |
| `settings/draw.rs:505` | same `.max(3)` guard | correct pattern |
| `help.rs:931` | `.clamp(8, 22)` | const, min ≤ max |

### u16 / usize subtractions on size/coords

| Site | Expression | Why SAFE |
|---|---|---|
| `viewport.rs:31` | `area.x + area.width - 1` | guarded `if area.width > 1` (line 30) |
| `lightbox.rs:15` | `area.width - LIGHTBOX_EDGE_INSET*2` | guarded by `if area.width <= INSET*2` above |
| `draw.rs:489` | `(total - prev) as u16` | guarded `if total > prev && prev > 0` |
| `draw.rs:867` | `total_w - used - version_span.content.len()` | guarded `if total_w > used + …` |
| `draw.rs:1088` | `dis_chars.len() - 1` | `dis_chars = &['▓','▒','░']` (const, len = 3) |
| `draw.rs:1288, 1304` | `cur_row = total_rows - 1` | `total_rows: u16 = 1` initially, only `+=` |
| `draw.rs:1312` | `cursor_row - visible_lines + 1` | inside `if cursor_row >= visible_lines` |
| `draw.rs:1323` | `cursor_row - input_scroll` | `input_scroll ≤ cursor_row` by construction |
| `markdown.rs:349` | `j < num_cols - 1` | inside `for j in 0..num_cols` — unreachable when `num_cols == 0` |
| `markdown.rs:358, 377, 387` | `i < rows.len() - 1` | inside `for (i, _) in rows.iter().enumerate()`; `rows` non‑empty (checked at 222) |
| `markdown.rs:674` | `TAB_WIDTH - (col % TAB_WIDTH)` | `(col % TAB_WIDTH) < TAB_WIDTH` const |
| `help.rs:227, 229` | `rows.len() - 1` | guarded `if rows.is_empty() { return; }` above |
| `models/input.rs:32, 80, 124` | `len - 1` patterns | guarded by `> 0` / `== 0` checks |

### Division / modulo

| Site | Expression | Why SAFE |
|---|---|---|
| `draw.rs:105, 758, 1128` | `% SPINNER_FRAMES.len()` | const, non‑empty |
| `draw.rs:257` | `% (WIDTH + CHARS.len())` | const non‑empty |
| `draw.rs:957, 990, 1069` | `msg_area.height / 2`, `(total_block as u16) / 2` | u16 / const, never 0 divisor |
| `markdown.rs:283` | `w * available / total_content` | guarded `if total_content > available`; `col_widths` init `vec![3; num_cols]` ⇒ `> 0` once `num_cols > 0` |
| `markdown.rs:288` | `… / shrinkable_total` | reached only when `shrinkable_indices` non‑empty AND each w > 12 ⇒ `shrinkable_total ≥ 13` |
| `render.rs:65, 213, 291, 432` | `% braille.len()`, `% SPINNER_FRAMES.len()` | const non‑empty |

### Indexing into split layouts

| Site | Layout slots | Used indices | Verdict |
|---|---|---|---|
| `draw.rs:745‑755 → outer` | 6 constraints | `outer[0..=5]` | in range |
| `draw.rs:1353 → footer_chunks` | 2 constraints | `[0],[1]` | in range |
| `plugins/draw.rs:67‑75 → outer/panes` | 2 / 2 | `[0],[1]` | in range |
| `settings/draw.rs:30‑38 → outer/panes` | 2 / 2 | `[0],[1]` | in range |
| `settings/draw.rs:467‑471 → split` | 2 | `[0],[1]` | in range |
| `models/mod.rs:611‑620 → chunks` | 5 | `chunks[0..=4]` | in range |
| `models/mod.rs:740‑747 → chunks` | 3 | `chunks[0..=2]` | in range |

`Layout::split` returns its requested rect count even on a 0‑sized parent
(possibly with zero‑height/width rects). Indexing is length‑safe.

### `.unwrap()` / `.expect()` on the render path

| Site | Verdict |
|---|---|
| `draw.rs:948` `art_display_widths.iter()…max().unwrap_or(0)` | `unwrap_or` |
| `markdown.rs:227, 329` | `unwrap_or` |
| `highlight.rs:172` | inside `.map()` over `find()` result — not on a panic path |
| `sidecar.rs:322, 340, 362` | `#[cfg(test)]` blocks |
| `markdown.rs:807, 885, 886` | `#[cfg(test)]` |
| `models/mod.rs:934, 987, 991`, `input.rs:183` | `#[cfg(test)]` |
| `help.rs:409` | one‑time `assets/help.json` deserialization at startup, not size‑dependent |

### Rect construction

| Site | Verdict |
|---|---|
| `draw.rs:452` (`msg_area`) | constraint‑driven, fits in `frame.area()` |
| `draw.rs:505` (`msg_inner`) | `saturating_sub(2)` on w/h, `+1` on x/y inside frame |
| `draw.rs:1000, 1032, 1053, 1096` (logo cells) | inside `if avail_h >= total_block && avail_w >= max_art_width + 2` guard |
| `draw.rs:1117` (indicator) | `width = msg_area.width, height = 1` — fits |
| `lightbox.rs`, `toast.rs::toast_rect` | guarded explicit `saturating_sub`, height/width clamped to `safe.*` |
| `models/mod.rs:715‑724` (expanded lightbox) | **explicitly guarded**: `if width < 20 || height < 6 { return; }` |

### Files with **no** render‑layout math (clean)

- `sidecar.rs` — pure state glue, no `Rect`/`Frame`.
- `highlight.rs` — color/syntax only, no terminal geometry.
- `agent-engine/src/help.rs` — string rendering only; layout only in test code.

---

## Canonical patterns to replicate when adding new popups

1. `content.saturating_add(K).clamp(MIN.min(max_dim), max_dim)` (draw.rs:1552)
2. `needed.clamp(MIN, area.dim.saturating_sub(P).max(MIN))` (settings/draw.rs:455)
3. Early `if width < MIN || height < MIN { return; }` for opt‑in modals
   that shouldn't render below a threshold (models/mod.rs:719).

Pick (3) for opt‑in modals, (1)/(2) for must‑render toasts/inputs.

---

## Closing the case

The resize‑crash class is **dead on `dev`**. The remaining MEDIUM items
are cosmetic clipping when a user shrinks the terminal narrower than a
modal's hard‑min, which ratatui handles without panicking. Worth fixing
for polish, not for stability.
