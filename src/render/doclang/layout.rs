//! Spatial layout for DocLang export: flat extracted items are grouped
//! into lines and merged into paragraph-like blocks in reading order, and
//! each block picks up its semantic kind from the PDF structure tree (or a
//! conservative font-size heuristic when the document is untagged).
//!
//! List regions are recognized geometrically: a run of lines whose first
//! item is or begins with a marker glyph (`•`, `-`, `1.`, `a)`, `(iv)`)
//! becomes a `<list>` piece — markers are preserved, deeper indentation
//! nests lists, and a run that fails validation falls back to `<text>`.
//!
//! Item coordinates stay in PDF space (bottom-left origin) here; the
//! markup stage normalizes them into DocLang locations.

use pdf_inspector::TextItem;
use pdf_inspector::types::ItemType;
use std::collections::HashMap;

/// A bounding box in PDF space: `[x0, y0, x1, y1]`, bottom-left origin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl BBox {
    /// The item's box (x/y are the PDF lower-left origin already).
    pub fn of(item: &TextItem) -> BBox {
        BBox { x0: item.x, y0: item.y, x1: item.x + item.width, y1: item.y + item.height }
    }

    pub(crate) fn union(&mut self, other: BBox) {
        self.x0 = self.x0.min(other.x0);
        self.y0 = self.y0.min(other.y0);
        self.x1 = self.x1.max(other.x1);
        self.y1 = self.y1.max(other.y1);
    }

    /// True when `point` falls inside the box.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// One laid-out element of a page, in reading order.
pub(crate) enum Piece {
    /// A block of merged text lines, emitted as `<text>`, `<heading>`, or
    /// `<footnote>`.
    Text(TextBlock),
    /// An AcroForm field item (text carries `name: value`), emitted as a
    /// `<field_item>`; consecutive fields nest under one `<field_region>`.
    Field(TextItem),
    /// An image placeholder item, emitted as `<picture>`. `name` is the
    /// XObject resource name pdf-inspector records in `[Image: name]`.
    /// `full_page` is set by the caller once the page context is known
    /// (dominant image → `full_page_image` label + `pages/` part).
    Picture { item: TextItem, name: String, full_page: bool },
    /// A detected list region, emitted as `<list>` with `<ldiv>` items.
    List(ListBlock),
    /// A detected table region, emitted as `<table>` with OTSL cell tokens.
    Table(TableBlock),
}

impl Piece {
    pub fn bbox(&self) -> BBox {
        match self {
            Piece::Text(b) => b.bbox,
            Piece::List(b) => b.bbox,
            Piece::Table(b) => b.bbox,
            Piece::Field(i) | Piece::Picture { item: i, .. } => BBox::of(i),
        }
    }
}

/// A paragraph-like block of same-role lines.
pub(crate) struct TextBlock {
    /// Items in reading order (lines top-down, items left-right).
    pub items: Vec<TextItem>,
    /// Heading level when the block classifies as a heading.
    pub level: Option<u8>,
    /// Emitted as `<footnote>` instead of `<text>`.
    pub footnote: bool,
    /// Supplementary content (page numbers, artifacts): `<layer
    /// value="furniture"/>`.
    pub furniture: bool,
    /// The block's union box.
    pub bbox: BBox,
}

/// A detected list block; emitted as `<list>` (`class="ordered"` when the
/// markers count).
pub(crate) struct ListBlock {
    /// Ordered markers (`1.`, `a)`, `iv.`) vs bullet glyphs.
    pub ordered: bool,
    /// Items in reading order.
    pub items: Vec<ListItem>,
    /// The block's union box, including nested lists.
    pub bbox: BBox,
}

/// One list item: its marker, the text pieces that follow it, and any
/// nested list the item carries.
pub(crate) struct ListItem {
    /// The marker text as printed (`•`, `1.`, `a)`).
    pub marker: String,
    /// The marker's box (estimated for markers fused into the body item).
    pub marker_bbox: BBox,
    /// Item text after the marker, in reading order.
    pub body: Vec<TextItem>,
    /// Lists nested inside this item, in reading order.
    pub nested: Vec<ListBlock>,
}

/// A detected table region; emitted as `<table>` with OTSL cell tokens.
pub(crate) struct TableBlock {
    /// Grid cells in row-major order. Span-continuation cells
    /// (`lcel`/`ucel`/`xcel`) hold no items — their content lives in the
    /// span's leading cell.
    pub cells: Vec<Vec<TableCell>>,
    /// The block's union box.
    pub bbox: BBox,
}

/// One grid cell: an OTSL token plus the text items inside it.
pub(crate) struct TableCell {
    /// The OTSL separator token emitted before the cell's content.
    pub token: CellToken,
    /// Cell content in reading order (empty for `ecel` and continuations).
    pub items: Vec<TextItem>,
    /// The cell's grid box (continuations keep their own box).
    pub bbox: BBox,
}

/// OTSL cell-token flavor emitted before a cell's content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum CellToken {
    /// `fcel`: filled body cell.
    Fcel,
    /// `ecel`: empty body cell.
    Ecel,
    /// `ched`: column-header cell.
    Ched,
    /// `rhed`: row-header cell.
    Rhed,
    /// `corn`: corner cell of a double-header table.
    Corn,
    /// `lcel`: continuation of a horizontal span.
    Lcel,
    /// `ucel`: continuation of a vertical span.
    Ucel,
    /// `xcel`: continuation of a two-direction span.
    Xcel,
}

/// One extracted line (items sharing a baseline).
struct Line {
    items: Vec<TextItem>,
    bbox: BBox,
}

/// Structure-tree role lookup keyed by `(page, mcid)`.
pub(crate) type Roles = HashMap<(u32, i64), String>;

/// The link-annotation boxes of a page: `(bbox, uri)`.
pub(crate) fn link_rects(items: &[TextItem]) -> Vec<(BBox, String)> {
    items
        .iter()
        .filter_map(|i| match &i.item_type {
            ItemType::Link(uri) => Some((BBox::of(i), uri.clone())),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct VerticalSep {
    x: f32,
    y0: f32,
    y1: f32,
}

fn detect_vertical_rules(
    rules: &[crate::render::doclang::tables::Rule],
    textish: &[TextItem],
    initial_pieces: &[Piece],
) -> Vec<VerticalSep> {
    let mut v_seps: Vec<VerticalSep> = Vec::new();
    for r in rules {
        if !r.vertical {
            continue;
        }
        let len = r.hi - r.lo;
        if len < 30.0 {
            continue;
        }
        let inside_table = initial_pieces.iter().any(|p| {
            if let Piece::Table(t) = p {
                t.bbox.contains(r.pos, (r.lo + r.hi) / 2.0)
            } else {
                false
            }
        });
        if inside_table {
            continue;
        }
        let y_lo = r.lo - 15.0;
        let y_hi = r.hi + 15.0;
        let has_left = textish.iter().any(|it| {
            let mid_y = it.y + it.height * 0.5;
            mid_y >= y_lo
                && mid_y <= y_hi
                && (it.x + it.width) <= r.pos + 5.0
                && it.x >= r.pos - 400.0
        });
        let has_right = textish.iter().any(|it| {
            let mid_y = it.y + it.height * 0.5;
            mid_y >= y_lo
                && mid_y <= y_hi
                && it.x >= r.pos - 5.0
                && (it.x + it.width) <= r.pos + 400.0
        });
        if !has_left || !has_right {
            continue;
        }
        let crossing_count = textish
            .iter()
            .filter(|it| {
                let mid_y = it.y + it.height * 0.5;
                mid_y >= r.lo
                    && mid_y <= r.hi
                    && it.x < r.pos - 5.0
                    && (it.x + it.width) > r.pos + 5.0
            })
            .count();
        if crossing_count <= 1 {
            v_seps.push(VerticalSep { x: r.pos, y0: r.lo, y1: r.hi });
        }
    }
    v_seps
}

fn detect_whitespace_gutters(textish: &[TextItem]) -> Vec<VerticalSep> {
    if textish.len() < 4 {
        return Vec::new();
    }
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    for it in textish {
        min_x = min_x.min(it.x);
        max_x = max_x.max(it.x + it.width);
    }
    let total_w = max_x - min_x;
    if total_w < 100.0 {
        return Vec::new();
    }

    let mut best_sep: Option<VerticalSep> = None;
    let mut best_score = 0.0f32;

    let mut candidate_x = min_x + 0.25 * total_w;
    while candidate_x <= min_x + 0.75 * total_w {
        let mut left_items: Vec<&TextItem> = Vec::new();
        let mut right_items: Vec<&TextItem> = Vec::new();

        let mut max_left_x = f32::MIN;
        let mut min_right_x = f32::MAX;

        for it in textish {
            let x0 = it.x;
            let x1 = it.x + it.width;
            if x1 <= candidate_x {
                max_left_x = max_left_x.max(x1);
                left_items.push(it);
            } else if x0 >= candidate_x {
                min_right_x = min_right_x.min(x0);
                right_items.push(it);
            }
        }

        if left_items.len() >= 2 && right_items.len() >= 2 && min_right_x > max_left_x {
            let gap_w = min_right_x - max_left_x;
            if gap_w >= 12.0 {
                let left_y_min = left_items.iter().map(|it| it.y).fold(f32::MAX, f32::min);
                let left_y_max =
                    left_items.iter().map(|it| it.y + it.height).fold(f32::MIN, f32::max);
                let right_y_min = right_items.iter().map(|it| it.y).fold(f32::MAX, f32::min);
                let right_y_max =
                    right_items.iter().map(|it| it.y + it.height).fold(f32::MIN, f32::max);

                let overlap_y0 = left_y_min.max(right_y_min);
                let overlap_y1 = left_y_max.min(right_y_max);

                if overlap_y1 - overlap_y0 >= 25.0 {
                    let crossing = textish.iter().any(|it| {
                        let y0 = it.y;
                        let y1 = it.y + it.height;
                        let y_overlap = y0 < overlap_y1 && y1 > overlap_y0;
                        y_overlap && it.x < min_right_x && (it.x + it.width) > max_left_x
                    });
                    if !crossing {
                        let split_x = (max_left_x + min_right_x) * 0.5;
                        let score = (overlap_y1 - overlap_y0) * gap_w.min(50.0);
                        if score > best_score {
                            best_score = score;
                            best_sep =
                                Some(VerticalSep { x: split_x, y0: overlap_y0, y1: overlap_y1 });
                        }
                    }
                }
            }
        }
        candidate_x += 5.0;
    }

    best_sep.into_iter().collect()
}

fn layout_text_stream(
    items: Vec<TextItem>,
    roles: &Roles,
    tagged: bool,
    hints: &crate::DoclangHints,
    median: f32,
) -> Vec<Piece> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut pieces = Vec::new();
    let mut run: Vec<(Line, Option<Marker>)> = Vec::new();
    let mut run_prev: Option<BBox> = None;
    let mut run_indent = 0.0f32;
    for line in lines(items) {
        let probe = if hints.lists { probe_marker(&line) } else { None };
        match probe {
            Some(marker) => {
                if let Some(prev) = run_prev
                    && prev.y0 - line.bbox.y1 > 1.5 * line_height(&line)
                {
                    flush_run(&mut run, &mut pieces, roles, tagged, median);
                }
                run_indent = marker.indent_x;
                run_prev = Some(line.bbox);
                run.push((line, Some(marker)));
            }
            None => {
                let continues = run_prev.is_some_and(|prev| {
                    let gap = prev.y0 - line.bbox.y1;
                    line.bbox.x0 > run_indent + INDENT_TOL
                        && gap <= 1.5 * line_height(&line)
                        && gap >= -line_height(&line)
                });
                if continues {
                    run_prev = Some(line.bbox);
                    run.push((line, None));
                } else {
                    flush_run(&mut run, &mut pieces, roles, tagged, median);
                    run_prev = None;
                    merge_line(&mut pieces, line, roles, tagged, median);
                }
            }
        }
    }
    flush_run(&mut run, &mut pieces, roles, tagged, median);
    pieces
}

/// Lay out one page's items into pieces in reading order. `tagged` tells
/// whether the document carries a structure tree at all; untagged pages get
/// the font-size heading heuristic. `hints` gate list detection; `tables`
/// carries this page's tagged tables and drawn rules for table detection.
pub(crate) fn layout(
    items: Vec<TextItem>,
    roles: &Roles,
    tagged: bool,
    hints: &crate::DoclangHints,
    tables: &super::tables::TableCtx,
) -> Vec<Piece> {
    let median = median_font(&items);
    let mut textish: Vec<TextItem> = Vec::new();
    let mut initial_pieces: Vec<Piece> = Vec::new();
    for item in items {
        match &item.item_type {
            ItemType::Image => {
                let name = item
                    .text
                    .strip_prefix("[Image: ")
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or("")
                    .to_string();
                initial_pieces.push(Piece::Picture { item, name, full_page: false });
            }
            ItemType::FormField => initial_pieces.push(Piece::Field(item)),
            ItemType::Link(_) => {}
            ItemType::Text => textish.push(item),
        }
    }
    if hints.tables {
        for block in super::tables::detect(&mut textish, tables) {
            initial_pieces.push(Piece::Table(block));
        }
    }

    let v_seps = {
        let rules_seps = detect_vertical_rules(tables.rules, &textish, &initial_pieces);
        if !rules_seps.is_empty() { rules_seps } else { detect_whitespace_gutters(&textish) }
    };

    if v_seps.is_empty() {
        let mut pieces = initial_pieces;
        let mut text_pieces = layout_text_stream(textish, roles, tagged, hints, median);
        pieces.append(&mut text_pieces);
        pieces.sort_by(|a, b| {
            let (a, b) = (a.bbox(), b.bbox());
            b.y1.partial_cmp(&a.y1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
        });
        return pieces;
    }

    let sep = v_seps[0];
    let sep_y0 = sep.y0;
    let sep_y1 = sep.y1;

    let mut top_items: Vec<TextItem> = Vec::new();
    let mut bottom_items: Vec<TextItem> = Vec::new();
    let mut left_items: Vec<TextItem> = Vec::new();
    let mut right_items: Vec<TextItem> = Vec::new();

    for it in textish {
        let mid_y = it.y + it.height * 0.5;
        if mid_y > sep_y1 + 2.0 {
            top_items.push(it);
        } else if mid_y < sep_y0 - 2.0 {
            bottom_items.push(it);
        } else if (it.x + it.width) <= sep.x + 5.0 {
            left_items.push(it);
        } else if it.x >= sep.x - 5.0 {
            right_items.push(it);
        } else if mid_y > (sep_y0 + sep_y1) * 0.5 {
            top_items.push(it);
        } else {
            bottom_items.push(it);
        }
    }

    let mut top_pieces: Vec<Piece> = Vec::new();
    let mut bottom_pieces: Vec<Piece> = Vec::new();
    let mut left_pieces: Vec<Piece> = Vec::new();
    let mut right_pieces: Vec<Piece> = Vec::new();

    for p in initial_pieces {
        let b = p.bbox();
        let mid_y = (b.y0 + b.y1) * 0.5;
        if mid_y > sep_y1 + 2.0 {
            top_pieces.push(p);
        } else if mid_y < sep_y0 - 2.0 {
            bottom_pieces.push(p);
        } else if b.x1 <= sep.x + 5.0 {
            left_pieces.push(p);
        } else if b.x0 >= sep.x - 5.0 {
            right_pieces.push(p);
        } else if mid_y > (sep_y0 + sep_y1) * 0.5 {
            top_pieces.push(p);
        } else {
            bottom_pieces.push(p);
        }
    }

    let mut top_out = layout_text_stream(top_items, roles, tagged, hints, median);
    top_out.append(&mut top_pieces);
    top_out.sort_by(|a, b| {
        let (a, b) = (a.bbox(), b.bbox());
        b.y1.partial_cmp(&a.y1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut left_out = layout_text_stream(left_items, roles, tagged, hints, median);
    left_out.append(&mut left_pieces);
    left_out.sort_by(|a, b| {
        let (a, b) = (a.bbox(), b.bbox());
        b.y1.partial_cmp(&a.y1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut right_out = layout_text_stream(right_items, roles, tagged, hints, median);
    right_out.append(&mut right_pieces);
    right_out.sort_by(|a, b| {
        let (a, b) = (a.bbox(), b.bbox());
        b.y1.partial_cmp(&a.y1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut bottom_out = layout_text_stream(bottom_items, roles, tagged, hints, median);
    bottom_out.append(&mut bottom_pieces);
    bottom_out.sort_by(|a, b| {
        let (a, b) = (a.bbox(), b.bbox());
        b.y1.partial_cmp(&a.y1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let is_rtl = {
        let mut rtl = 0;
        let mut ltr = 0;
        for p in left_out.iter().chain(right_out.iter()) {
            if let Piece::Text(b) = p {
                for it in &b.items {
                    for ch in it.text.chars() {
                        if ('\u{0600}'..='\u{06FF}').contains(&ch)
                            || ('\u{0590}'..='\u{05FF}').contains(&ch)
                        {
                            rtl += 1;
                        } else if ch.is_alphabetic() {
                            ltr += 1;
                        }
                    }
                }
            }
        }
        rtl > ltr
    };

    let mut final_pieces = Vec::new();
    final_pieces.extend(top_out);
    if is_rtl {
        final_pieces.extend(right_out);
        final_pieces.extend(left_out);
    } else {
        final_pieces.extend(left_out);
        final_pieces.extend(right_out);
    }
    final_pieces.extend(bottom_out);

    final_pieces
}

/// Group items into lines by baseline proximity, each line sorted
/// left-to-right, lines produced top-down.
fn lines(mut items: Vec<TextItem>) -> Vec<Line> {
    items.sort_by(|a, b| {
        b.y.partial_cmp(&a.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut lines: Vec<Line> = Vec::new();
    for item in items {
        let bbox = BBox::of(&item);
        let fits = lines.last_mut().is_some_and(|line| {
            let base = line.items.first().map(|i| i.y).unwrap_or(item.y);
            (item.y - base).abs() <= 0.5 * item.height.max(line_height(line)).max(1.0)
        });
        if fits {
            let line = lines.last_mut().unwrap();
            line.bbox.union(bbox);
            line.items.push(item);
        } else {
            lines.push(Line { items: vec![item], bbox });
        }
    }
    for line in &mut lines {
        line.items.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
    }
    lines
}

fn line_height(line: &Line) -> f32 {
    line.items.iter().map(|i| i.height).fold(0.0_f32, f32::max)
}

/// Merge a line into the trailing text block when it reads as a
/// continuation, else start a new piece.
fn merge_line(pieces: &mut Vec<Piece>, line: Line, roles: &Roles, tagged: bool, median: f32) {
    let cls = classify(&line.items, roles, tagged, median);
    if let Some(Piece::Text(block)) = pieces.last_mut() {
        let gap = block.bbox.y0 - line.bbox.y1;
        // Furniture merges only with furniture — ordinary content must
        // never land inside an Artifact block's `<layer value="furniture"/>`.
        let same_kind = block.level == cls.level
            && block.footnote == cls.footnote
            && block.furniture == cls.furniture;
        // A continuation sits directly below the block and shares its
        // column; overlapping or side-by-side lines (multi-column text,
        // captions) start a new piece instead.
        let overlapping_x = line.bbox.x0 < block.bbox.x1 && line.bbox.x1 > block.bbox.x0;
        if same_kind && gap > 0.0 && gap <= 1.5 * line_height(&line) && overlapping_x {
            block.bbox.union(line.bbox);
            block.items.extend(line.items);
            return;
        }
    }
    pieces.push(Piece::Text(TextBlock {
        items: line.items,
        level: cls.level,
        footnote: cls.footnote,
        furniture: cls.furniture,
        bbox: line.bbox,
    }));
}

struct Classified {
    level: Option<u8>,
    footnote: bool,
    furniture: bool,
}

/// Semantic kind for a line: structure-tree roles win; when the document
/// is untagged a font-size heuristic catches display headings.
fn classify(items: &[TextItem], roles: &Roles, tagged: bool, median: f32) -> Classified {
    let mut level = None;
    let mut footnote = false;
    let mut furniture = false;
    for item in items {
        let Some(role) = item.mcid.and_then(|m| roles.get(&(item.page, m))) else {
            continue;
        };
        match role.as_str() {
            "Title" => level = Some(1),
            r if r.starts_with('H') => {
                if let Ok(n) = r[1..].parse::<u8>() {
                    let n = n.clamp(1, 6);
                    level = Some(level.map_or(n, |l: u8| l.min(n)));
                }
            }
            "Note" => footnote = true,
            "Artifact" => furniture = true,
            _ => {}
        }
    }
    if level.is_none() && !footnote && !furniture && !tagged {
        level = heuristic_heading(items, median);
    }
    Classified { level, footnote, furniture }
}

/// Untagged-document fallback: a line set markedly larger than the page's
/// median text is probably a heading.
fn heuristic_heading(items: &[TextItem], median: f32) -> Option<u8> {
    let size = items.iter().map(|i| i.font_size).fold(0.0_f32, f32::max);
    let ratio = size / median.max(1.0);
    if ratio >= 1.8 {
        Some(1)
    } else if ratio >= 1.4 {
        Some(2)
    } else {
        None
    }
}

/// Median font size of real text items on a page (heading heuristic base).
fn median_font(items: &[TextItem]) -> f32 {
    let mut sizes: Vec<f32> = items
        .iter()
        .filter(|i| i.font_size > 0.5 && matches!(i.item_type, ItemType::Text))
        .map(|i| i.font_size)
        .collect();
    if sizes.is_empty() {
        return 12.0;
    }
    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sizes[sizes.len() / 2]
}

// ---- List detection -----------------------------------------------------

/// Indent tolerance assigning a marker to a nesting level (points).
const INDENT_TOL: f32 = 3.0;

/// Unambiguous bullet glyphs.
const BULLETS: &[char] = &['•', '◦', '▪', '▫', '‣', '·', '∙', '●', '○', '□', '■'];

/// ASCII/dash glyphs that count as bullets only when space-separated —
/// they are too common mid-word otherwise.
const SOFT_BULLETS: &[char] = &['-', '–', '—', '*'];

/// Marker flavor: bulleted or counted.
#[derive(Clone, Copy, PartialEq)]
enum ListKind {
    Bullet,
    Ordered,
}

/// A line's leading list marker and the items after it.
struct Marker {
    kind: ListKind,
    /// Ordering candidates: `i` counts as both alpha 9 and roman 1; the
    /// sequence check keeps whichever fits the run.
    ordinals: Vec<u32>,
    /// The marker as printed (`•`, `1.`, `a)`).
    text: String,
    /// Marker box (estimated for markers fused into the body item).
    bbox: BBox,
    /// The marker's left edge; the indent-level key.
    indent_x: f32,
    /// The line's items with the marker split off.
    body: Vec<TextItem>,
}

/// The marker at a line's start, if the first item is or begins with one.
fn probe_marker(line: &Line) -> Option<Marker> {
    let first = line.items.first()?;
    let text = first.text.trim_start();
    if text.is_empty() {
        return None;
    }
    // Standalone marker: the whole first item is the marker glyph.
    if let Some((kind, ordinals)) = classify_marker(text) {
        let body = if line.items.len() > 1 { line.items[1..].to_vec() } else { Vec::new() };
        return Some(Marker {
            kind,
            ordinals,
            text: text.to_string(),
            bbox: BBox::of(first),
            indent_x: first.x,
            body,
        });
    }
    // Fused marker: the marker is a prefix of the item (`1.First`).
    let (marker_len, kind, ordinals) = marker_prefix(text)?;
    let marker = &text[..marker_len];
    // No measurement of the marker's drawn width; estimate from glyphs.
    let est_w = (marker.chars().count() as f32 * first.font_size.max(1.0) * 0.55).min(first.width);
    let mut body_item = first.clone();
    body_item.text = text[marker_len..].trim_start().to_string();
    let mut body = vec![body_item];
    body.extend(line.items[1..].iter().cloned());
    Some(Marker {
        kind,
        ordinals,
        text: marker.to_string(),
        bbox: BBox { x0: first.x, y0: first.y, x1: first.x + est_w, y1: first.y + first.height },
        indent_x: first.x,
        body,
    })
}

/// The marker a whole-item text spells (`•`, `1.`, `(a)`), if any.
fn classify_marker(text: &str) -> Option<(ListKind, Vec<u32>)> {
    if text.chars().count() == 1 {
        let c = text.chars().next()?;
        if BULLETS.contains(&c) || SOFT_BULLETS.contains(&c) {
            return Some((ListKind::Bullet, Vec::new()));
        }
    }
    ordered_token(text).map(|o| (ListKind::Ordered, o))
}

/// The marker prefix inside a fused item (`1.First` → `1.` + `First`).
/// Returns the marker's byte length, kind, and ordinal candidates.
fn marker_prefix(text: &str) -> Option<(usize, ListKind, Vec<u32>)> {
    let c = text.chars().next()?;
    if BULLETS.contains(&c)
        || (SOFT_BULLETS.contains(&c) && text[c.len_utf8()..].starts_with(char::is_whitespace))
    {
        return Some((c.len_utf8(), ListKind::Bullet, Vec::new()));
    }
    // Parenthesized ordered markers fused to their text: `(iv)Next`.
    if c == '(' {
        let close = text.find(')')?;
        let rest = &text[close + 1..];
        return if rest.is_empty() {
            None
        } else {
            ordered_token(&text[..=close]).map(|o| (close + 1, ListKind::Ordered, o))
        };
    }
    let token_len: usize =
        text.chars().take_while(|c| c.is_ascii_alphanumeric()).map(char::len_utf8).sum();
    if token_len == 0 {
        return None;
    }
    let term = text[token_len..].chars().next()?;
    if term != '.' && term != ')' {
        return None;
    }
    let end = token_len + term.len_utf8();
    let rest = &text[end..];
    // `1.5` decimals and version strings are not markers; an empty tail
    // means the item is a standalone marker (handled above).
    if rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    ordered_token(&text[..end]).map(|o| (end, ListKind::Ordered, o))
}

/// Ordinal candidates for a whole-token ordered marker (`1.`, `a)`, `(iv)`).
fn ordered_token(text: &str) -> Option<Vec<u32>> {
    let inner = text
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .or_else(|| text.strip_suffix('.'))
        .or_else(|| text.strip_suffix(')'))?;
    if inner.is_empty() {
        return None;
    }
    if inner.chars().all(|c| c.is_ascii_digit()) {
        return inner.parse::<u32>().ok().filter(|n| (1..=9_999).contains(n)).map(|n| vec![n]);
    }
    if inner.chars().count() == 1 {
        let c = inner.chars().next()?.to_ascii_lowercase();
        if !c.is_ascii_lowercase() {
            return None;
        }
        let alpha = c as u32 - 'a' as u32 + 1;
        return Some(match roman(&inner.to_ascii_lowercase()) {
            Some(r) if r != alpha => vec![alpha, r],
            _ => vec![alpha],
        });
    }
    roman(&inner.to_ascii_lowercase()).map(|r| vec![r])
}

/// Decode a roman numeral; `None` for non-roman letters.
fn roman(text: &str) -> Option<u32> {
    let mut total = 0u32;
    let mut prev = 0u32;
    for c in text.chars() {
        let v = match c {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        };
        // Subtractive notation: a smaller value before a larger subtracts.
        total = total.saturating_add(v).saturating_sub(if v > prev { 2 * prev } else { 0 });
        prev = v;
    }
    (1..=3999).contains(&total).then_some(total)
}

/// Marker-style family for sequence validation: decimal, alpha, and roman
/// counts restart independently but may resume across other styles.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum OrdinalFamily {
    Decimal,
    Alpha,
    Roman,
}

/// The family an ordered marker's token belongs to.
fn ordinal_family(text: &str) -> OrdinalFamily {
    let inner = if let Some(t) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        t
    } else {
        text.strip_suffix('.').or_else(|| text.strip_suffix(')')).unwrap_or(text)
    };
    if inner.chars().all(|c| c.is_ascii_digit()) {
        OrdinalFamily::Decimal
    } else if inner.chars().count() == 1 {
        OrdinalFamily::Alpha
    } else {
        OrdinalFamily::Roman
    }
}

/// A run reads as a list when ≥2 markers share its shallowest indent and
/// its ordered markers count plausibly: each family's ordinals may
/// continue (`n+1`), repeat, or restart — but never skip forward.
fn valid_run(run: &[(Line, Option<Marker>)]) -> bool {
    let Some(min_indent) =
        run.iter().filter_map(|(_, p)| p.as_ref()).map(|m| m.indent_x).reduce(f32::min)
    else {
        return false;
    };
    let top: Vec<&Marker> = run
        .iter()
        .filter_map(|(_, p)| p.as_ref())
        .filter(|m| (m.indent_x - min_indent).abs() <= INDENT_TOL)
        .collect();
    if top.len() < 2 {
        return false;
    }
    // Ambiguous markers keep every plausible ordinal (`(i)` is alpha 9
    // and roman 1); committing to one — e.g. the minimum — would break
    // alpha runs like `(i) (j) (k)` and roman runs like `(i) (ii)`.
    let mut last: HashMap<OrdinalFamily, Vec<u32>> = HashMap::new();
    for marker in top.iter().filter(|m| m.kind == ListKind::Ordered) {
        let family = ordinal_family(&marker.text);
        let reached: Vec<u32> = match last.get(&family) {
            None => marker.ordinals.clone(),
            Some(prevs) => marker
                .ordinals
                .iter()
                .copied()
                .filter(|n| prevs.iter().any(|p| *n <= p + 1))
                .collect(),
        };
        if reached.is_empty() {
            return false;
        }
        last.insert(family, reached);
    }
    true
}

/// A list block under construction at one indent level.
struct OpenLevel {
    indent_x: f32,
    kind: ListKind,
    block: ListBlock,
}

impl OpenLevel {
    fn new(marker: Marker) -> OpenLevel {
        let bbox = marker.bbox;
        OpenLevel {
            indent_x: marker.indent_x,
            kind: marker.kind,
            block: ListBlock {
                ordered: marker.kind == ListKind::Ordered,
                items: vec![item_of(marker)],
                bbox,
            },
        }
    }
}

fn item_of(marker: Marker) -> ListItem {
    ListItem {
        marker: marker.text,
        marker_bbox: marker.bbox,
        body: marker.body,
        nested: Vec::new(),
    }
}

/// Close the deepest open level: attach it to its parent item, or emit it
/// as a top-level `<list>` piece.
fn close_level(stack: &mut Vec<OpenLevel>, pieces: &mut Vec<Piece>) {
    let Some(top) = stack.pop() else { return };
    match stack.last_mut() {
        Some(parent) => {
            parent.block.bbox.union(top.block.bbox);
            if let Some(item) = parent.block.items.last_mut() {
                item.nested.push(top.block);
            }
        }
        None => pieces.push(Piece::List(top.block)),
    }
}

/// Close a list run: validate it, then emit `<list>` pieces or fall back
/// to paragraph blocks.
fn flush_run(
    run: &mut Vec<(Line, Option<Marker>)>,
    pieces: &mut Vec<Piece>,
    roles: &Roles,
    tagged: bool,
    median: f32,
) {
    if run.is_empty() {
        return;
    }
    if valid_run(run) {
        emit_list_pieces(std::mem::take(run), pieces);
    } else {
        for (line, _) in run.drain(..) {
            merge_line(pieces, line, roles, tagged, median);
        }
    }
}

/// Emit a validated run as `<list>` pieces: same-indent markers extend the
/// open list, deeper indents nest under the current item, shallower ones
/// close levels, and a marker-kind change at a level splits the list.
fn emit_list_pieces(run: Vec<(Line, Option<Marker>)>, pieces: &mut Vec<Piece>) {
    // stack[0] is the top-level block; deeper entries nest into the item
    // above them when closed.
    let mut stack: Vec<OpenLevel> = Vec::new();
    for (line, probe) in run {
        match probe {
            None => {
                if let Some(item) = stack.last_mut().and_then(|l| l.block.items.last_mut()) {
                    item.body.extend(line.items);
                }
            }
            Some(marker) => {
                let indent = marker.indent_x;
                match stack.iter().position(|l| (l.indent_x - indent).abs() <= INDENT_TOL) {
                    Some(lvl) => {
                        while stack.len() > lvl + 1 {
                            close_level(&mut stack, pieces);
                        }
                        if stack[lvl].kind == marker.kind {
                            stack[lvl].block.items.push(item_of(marker));
                        } else {
                            // Same indent, new marker style → adjacent list.
                            close_level(&mut stack, pieces);
                            stack.push(OpenLevel::new(marker));
                        }
                    }
                    None => {
                        // A dedent past the root ends the run's list.
                        if stack.first().is_some_and(|root| indent < root.indent_x - INDENT_TOL) {
                            while !stack.is_empty() {
                                close_level(&mut stack, pieces);
                            }
                        }
                        stack.push(OpenLevel::new(marker));
                    }
                }
            }
        }
        if let Some(top) = stack.last_mut() {
            top.block.bbox.union(line.bbox);
        }
    }
    while !stack.is_empty() {
        close_level(&mut stack, pieces);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, x: f32, y: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width: text.len() as f32 * 6.0,
            height: 12.0,
            rotation: 0.0,
            advance_known: true,
            font: "F1".into(),
            font_tag: "F1".into(),
            legacy_symbol_rewrite: false,
            font_size: 12.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            font_weight: None,
            bold_source: None,
            fixed_pitch: None,
            fill_color: None,
            stroke_color: None,
            render_mode: None,
            is_underline: false,
            is_strikeout: false,
            item_type: ItemType::Text,
            mcid: None,
            baseline_shift: 0.0,
        }
    }

    /// One (text, x) per line, 14pt apart, top-down.
    fn lay(lines: &[(&str, f32)]) -> Vec<Piece> {
        let items: Vec<TextItem> = lines
            .iter()
            .enumerate()
            .map(|(i, (t, x))| item(t, *x, 700.0 - i as f32 * 14.0))
            .collect();
        let tables = crate::render::doclang::tables::TableCtx {
            tagged: &[],
            rules: &[],
            ruled: false,
            page: 1,
        };
        layout(items, &Roles::new(), false, &crate::DoclangHints::default(), &tables)
    }

    fn first_list(pieces: &[Piece]) -> &ListBlock {
        pieces
            .iter()
            .find_map(|p| match p {
                Piece::List(l) => Some(l),
                _ => None,
            })
            .expect("no list piece")
    }

    #[test]
    fn ordered_markers_build_a_list() {
        let pieces = lay(&[("1.First", 50.0), ("2.Second", 50.0), ("3.Third", 50.0)]);
        let list = first_list(&pieces);
        assert!(list.ordered);
        assert_eq!(list.items.len(), 3);
        assert_eq!(list.items[0].marker, "1.");
        assert_eq!(list.items[2].body[0].text, "Third");
    }

    #[test]
    fn bullets_build_an_unordered_list() {
        let pieces = lay(&[("•One", 50.0), ("•Two", 50.0)]);
        let list = first_list(&pieces);
        assert!(!list.ordered);
        assert_eq!(list.items[0].marker, "•");
    }

    #[test]
    fn deeper_indent_nests_under_the_item() {
        let pieces = lay(&[
            ("•Outer one", 50.0),
            ("◦Inner one", 80.0),
            ("◦Inner two", 80.0),
            ("•Outer two", 50.0),
        ]);
        let list = first_list(&pieces);
        assert_eq!(list.items.len(), 2);
        let nested = &list.items[0].nested;
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].items.len(), 2);
    }

    #[test]
    fn two_columns_with_vertical_rule_preserve_reading_order() {
        let items = vec![
            item("Main Title", 50.0, 750.0),
            item("Left line 1", 50.0, 700.0),
            item("Left line 2", 50.0, 680.0),
            item("Right line 1", 300.0, 700.0),
            item("Right line 2", 300.0, 680.0),
        ];
        let vrule = crate::render::doclang::tables::Rule {
            vertical: true,
            pos: 200.0,
            lo: 650.0,
            hi: 720.0,
        };
        let rules = vec![vrule];
        let tables = crate::render::doclang::tables::TableCtx {
            tagged: &[],
            rules: &rules,
            ruled: true,
            page: 1,
        };
        let pieces = layout(items, &Roles::new(), false, &crate::DoclangHints::default(), &tables);

        let texts: Vec<String> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Text(b) => {
                    Some(b.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>().join(" "))
                }
                _ => None,
            })
            .collect();

        // Must read Title -> Left column -> Right column
        assert_eq!(texts.len(), 3, "expected 3 distinct blocks, got {texts:?}");
        assert!(texts[0].contains("Main Title"), "first must be title, got {:?}", texts[0]);
        assert!(
            texts[1].contains("Left line 1") && texts[1].contains("Left line 2"),
            "second must be left column, got {:?}",
            texts[1]
        );
        assert!(
            !texts[1].contains("Right"),
            "left column must not contain right column text, got {:?}",
            texts[1]
        );
        assert!(
            texts[2].contains("Right line 1") && texts[2].contains("Right line 2"),
            "third must be right column, got {:?}",
            texts[2]
        );
        assert!(
            !texts[2].contains("Left"),
            "right column must not contain left column text, got {:?}",
            texts[2]
        );
    }

    #[test]
    fn two_columns_with_gutter_preserve_reading_order() {
        let items = vec![
            item("Header across top", 50.0, 750.0),
            item("Col A line 1", 50.0, 700.0),
            item("Col A line 2", 50.0, 680.0),
            item("Col B line 1", 320.0, 700.0),
            item("Col B line 2", 320.0, 680.0),
        ];
        let rules = Vec::new();
        let tables = crate::render::doclang::tables::TableCtx {
            tagged: &[],
            rules: &rules,
            ruled: false,
            page: 1,
        };
        let pieces = layout(items, &Roles::new(), false, &crate::DoclangHints::default(), &tables);

        let texts: Vec<String> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Text(b) => {
                    Some(b.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>().join(" "))
                }
                _ => None,
            })
            .collect();

        assert_eq!(texts.len(), 3, "expected 3 distinct blocks, got {texts:?}");
        assert!(texts[0].contains("Header"), "first must be header, got {:?}", texts[0]);
        assert!(texts[1].contains("Col A line 1"), "second must be column A, got {:?}", texts[1]);
        assert!(
            !texts[1].contains("Col B"),
            "column A must not contain column B text, got {:?}",
            texts[1]
        );
        assert!(texts[2].contains("Col B line 1"), "third must be column B, got {:?}", texts[2]);
    }

    #[test]
    fn a_paragraph_between_markers_splits_lists() {
        let pieces = lay(&[
            ("1.One", 50.0),
            ("2.Two", 50.0),
            ("Plain paragraph.", 50.0),
            ("3.Three", 50.0),
            ("4.Four", 50.0),
        ]);
        let lists = pieces.iter().filter(|p| matches!(p, Piece::List(_))).count();
        assert_eq!(lists, 2);
    }

    #[test]
    fn parenthesized_alpha_run_starting_at_i_stays_a_list() {
        // `(i)` is both alpha 9 and roman 1; the run must keep the
        // alpha reading so `(j) (k)` continue it.
        let pieces = lay(&[("(i)ninth", 50.0), ("(j)tenth", 50.0), ("(k)eleventh", 50.0)]);
        let list = first_list(&pieces);
        assert_eq!(list.items.len(), 3);
        assert_eq!(list.items[0].marker, "(i)");
    }

    #[test]
    fn a_lone_marker_stays_text() {
        let pieces = lay(&[("Regular text.", 50.0), ("1.Only line", 50.0), ("More text.", 50.0)]);
        assert!(pieces.iter().all(|p| !matches!(p, Piece::List(_))));
    }

    #[test]
    fn decimal_numbers_are_not_markers() {
        let pieces = lay(&[("Version 1.5 shipped", 50.0), ("then 1.6 followed", 50.0)]);
        assert!(pieces.iter().all(|p| !matches!(p, Piece::List(_))));
    }

    #[test]
    fn marker_styles_split_into_adjacent_lists() {
        let pieces =
            lay(&[("1.One", 50.0), ("2.Two", 50.0), ("•Bullet a", 50.0), ("•Bullet b", 50.0)]);
        let lists = pieces.iter().filter(|p| matches!(p, Piece::List(_))).count();
        assert_eq!(lists, 2);
    }
}
