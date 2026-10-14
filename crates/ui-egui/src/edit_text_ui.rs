//! Edit a PDF ▸ Edit text & images: boxes around the paragraphs and images already on the page.
//! Click a paragraph to edit it in place (⌘Enter or clicking away applies and rewraps it to the
//! box, Esc cancels); drag it to move it, or drag the handle on its left or right edge to rewrap
//! it to a new width. Click an image to select it: drag to move, drag a corner to resize (keeping
//! its proportions), right-click for rotate, flip, replace, save and delete; Delete removes it.

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::{Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Stroke};
use pdfcraft_engine::{Edit, ObjectKind, ObjectTarget};
use pdfcraft_render::DocInfo;

use crate::canvas::{DocView, PageXform};

const ACCENT: Color32 = Color32::from_rgb(0x14, 0x73, 0xE6);

/// A cached, read-only page background. The source glyphs are removed before rasterization;
/// colour, pictures and unselected text beneath the editor remain visible.
#[derive(Default)]
pub(crate) struct TextBackground {
    key: Option<(u64, usize, usize)>,
    renderer: Option<pdfcraft_render::RenderPool>,
    texture: Option<egui::TextureHandle>,
    tag: Option<u64>,
    received: Option<u64>,
    draft_key: Option<(String, pdfcraft_engine::BlockStyle)>,
    draft_rect: Option<[f64; 4]>,
}

impl TextBackground {
    pub fn pending(&self) -> bool {
        self.renderer.is_some() && self.received != self.tag
    }

    pub fn prepare(&mut self, ctx: &egui::Context, doc: &pdfcraft_engine::Document, editor: Option<&LineEditor>, scale: f32) {
        let key = editor.map(|ed| (doc.edit_generation(), ed.page, ed.block));
        if self.key != key {
            *self = Self { key, ..Default::default() };
            if let Some((_, page, block)) = key {
                self.renderer = doc.text_edit_preview(page, block).ok().map(|p| p.renderer());
            }
        }
        if let Some(ed) = editor {
            let draft_key = (ed.text.clone(), ed.style());
            if self.draft_key.as_ref() != Some(&draft_key) {
                self.draft_rect = doc.text_edit_layout(ed.page, ed.block, &ed.text, &draft_key.1).ok().and_then(|layout| layout.draft_rect);
                self.draft_key = Some(draft_key);
            }
        }
        let (Some((_, page, _)), Some(renderer)) = (self.key, self.renderer.as_ref()) else { return };
        let Some(p) = doc.info.pages.get(page) else { return };
        let max_side = ctx.input(|i| i.max_texture_side) as f32;
        let scale = scale.min(crate::canvas::BASE_SIDE.min(max_side) / p.width.max(p.height).max(1.0));
        let tag = (scale * 1000.0) as u64;
        if self.tag != Some(tag) {
            renderer.set_queue(vec![pdfcraft_render::RenderRequest { page, scale, tag, ..Default::default() }]);
            self.tag = Some(tag);
        }
        if let Some(out) = renderer.try_recv() {
            if out.error.is_some() {
                self.renderer = None;
                self.texture = None;
                return;
            }
            if out.request.tag == tag {
                self.texture = Some(ctx.load_texture(
                    "text-editor-background",
                    crate::canvas::texture_image([out.width as usize, out.height as usize], out.rgba),
                    egui::TextureOptions::LINEAR,
                ));
                self.received = Some(tag);
            }
        }
        if self.received != Some(tag) {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    pub fn ready(&self, page: usize, block: usize) -> bool {
        self.key.is_some_and(|(_, p, b)| (p, b) == (page, block)) && self.texture.is_some()
    }

    pub fn paint(&self, painter: &egui::Painter, xf: &PageXform, page: usize, editor: Option<&LineEditor>) {
        let Some(_) = editor.filter(|ed| ed.page == page && self.ready(page, ed.block)) else { return };
        let Some(tex) = &self.texture else { return };
        // Font metrics can underestimate the actual glyph ink. Paint the complete textless
        // page so overhanging glyphs cannot remain outside the estimated source rectangle.
        let painter = painter.with_clip_rect(painter.clip_rect().intersect(xf.rect));
        xf.paint_image(&painter, tex.id(), 0.0, 0.0, 1.0, 1.0);
    }
}

/// How many faint Edit-text outlines are drawn per page per frame. CAD plots can draw
/// ~1000 tiny images on one page; stroking all of them every frame (plus a hover query
/// per box) stalls the UI. Past this, only the selected and hovered boxes are outlined:
/// hit-testing still covers every image, so nothing becomes unselectable.
const MAX_OUTLINES: usize = 250;

/// A paragraph being edited.
#[derive(Clone, Debug, PartialEq)]
pub struct LineEditor {
    pub selection: Option<std::ops::Range<usize>>,
    pub font_resource: Option<String>,
    pub page: usize,
    pub block: usize,
    pub text: String,
    original: String,
    rect: Rect,
    /// The original PDF rectangle, in user space. It is converted again each frame so the editor
    /// stays attached while the page is zoomed, scrolled, or rotated.
    source_rect: [f32; 4],
    multiline: bool,
    source: Option<(pdfcraft_engine::TextBlock, Vec<pdfcraft_engine::TextLine>)>,
    pub(crate) fonts: BTreeMap<String, String>,
    /// The draft extends outside the source container; its frame follows the live draft bounds.
    pub overflow: bool,
    /// The initial source container width in screen pixels.
    max_width: f32,
    /// Current screen-space size. Recomputed from the current page transform before painting.
    size: f32,
    focus: bool,
    /// The Format text panel's values, and what the paragraph had (to send only changes).
    pub look: pdfcraft_engine::AddedText,
    look0: pdfcraft_engine::AddedText,
    /// Underline, line spacing (× size; 0 = the paragraph's own), character spacing (pt) and
    /// horizontal scale (%), and what they were.
    pub extras: Extras,
    extras0: Extras,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extras {
    pub underline: bool,
    pub line_spacing: f64,
    pub paragraph_spacing: f64,
    pub char_spacing: f64,
    pub scale: f64,
}

impl Default for Extras {
    fn default() -> Self {
        Extras { underline: false, line_spacing: 0.0, paragraph_spacing: 0.0, char_spacing: 0.0, scale: 100.0 }
    }
}

pub fn is_bulleted(text: &str) -> bool {
    let mut lines = text.lines();
    let Some(first) = lines.next() else { return false };
    first.trim_start().starts_with("• ") || first.trim_start().starts_with("- ")
}

pub fn is_numbered(text: &str) -> bool {
    let mut lines = text.lines();
    let Some(first) = lines.next() else { return false };
    let t = first.trim_start();
    let num_end = t.find('.').unwrap_or(0);
    num_end > 0 && t[..num_end].chars().all(|c| c.is_ascii_digit()) && t[num_end..].starts_with(". ")
}

pub fn toggle_bullet_list(text: &mut String) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return false;
    }
    let all_bulleted = lines.iter().all(|l| {
        let t = l.trim_start();
        t.is_empty() || t.starts_with("• ") || t.starts_with("- ")
    });
    let mut new_text = String::with_capacity(text.len() + 16);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            new_text.push('\n');
        }
        let trimmed = line.trim_start();
        if all_bulleted {
            if let Some(rest) = trimmed.strip_prefix("• ").or_else(|| trimmed.strip_prefix("- ")) {
                new_text.push_str(rest);
            } else {
                new_text.push_str(line);
            }
        } else if trimmed.is_empty() || trimmed.starts_with("• ") {
            new_text.push_str(line);
        } else {
            new_text.push_str("• ");
            new_text.push_str(line);
        }
    }
    if new_text != *text {
        *text = new_text;
        true
    } else {
        false
    }
}

pub fn toggle_numbered_list(text: &mut String) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return false;
    }
    let is_num_line = |l: &str| {
        let t = l.trim_start();
        if t.is_empty() {
            return true;
        }
        let dot_idx = t.find('.').unwrap_or(0);
        dot_idx > 0 && t[..dot_idx].chars().all(|c| c.is_ascii_digit()) && t[dot_idx..].starts_with(". ")
    };
    let all_numbered = lines.iter().all(|l| is_num_line(l));
    let mut new_text = String::with_capacity(text.len() + 16);
    let mut num = 1;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            new_text.push('\n');
        }
        let t = line.trim_start();
        if all_numbered {
            if let Some(dot_idx) = t.find('.')
                && dot_idx > 0
                && t[..dot_idx].chars().all(|c| c.is_ascii_digit())
                && t[dot_idx..].starts_with(". ")
            {
                new_text.push_str(&t[dot_idx + 2..]);
                continue;
            }
            new_text.push_str(line);
        } else if t.is_empty() {
            new_text.push_str(line);
        } else {
            use std::fmt::Write as _;
            let _ = write!(new_text, "{num}. ");
            new_text.push_str(t);
            num += 1;
        }
    }
    if new_text != *text {
        *text = new_text;
        true
    } else {
        false
    }
}

pub fn char_to_superscript(c: char) -> char {
    match c {
        '0' => '⁰', '1' => '¹', '2' => '²', '3' => '³', '4' => '⁴',
        '5' => '⁵', '6' => '⁶', '7' => '⁷', '8' => '⁸', '9' => '⁹',
        '+' => '⁺', '-' | '−' => '⁻', '=' => '⁼', '(' => '⁽', ')' => '⁾',
        'a' => 'ᵃ', 'b' => 'ᵇ', 'c' => 'ᶜ', 'd' => 'ᵈ', 'e' => 'ᵉ',
        'f' => 'ᶠ', 'g' => 'ᵍ', 'h' => 'ʰ', 'i' => 'ⁱ', 'j' => 'ʲ',
        'k' => 'ᵏ', 'l' => 'ˡ', 'm' => 'ᵐ', 'n' => 'ⁿ', 'o' => 'ᵒ',
        'p' => 'ᵖ', 'r' => 'ʳ', 's' => 'ˢ', 't' => 'ᵗ', 'u' => 'ᵘ',
        'v' => 'ᵛ', 'w' => 'ʷ', 'x' => 'ˣ', 'y' => 'ʸ', 'z' => 'ᶻ',
        'A' => 'ᴬ', 'B' => 'ᴮ', 'D' => 'ᴰ', 'E' => 'ᴱ', 'G' => 'ᴳ',
        'H' => 'ᴴ', 'I' => 'ᴵ', 'J' => 'ᴶ', 'K' => 'ᴷ', 'L' => 'ᴸ',
        'M' => 'ᴹ', 'N' => 'ᴺ', 'O' => 'ᴼ', 'P' => 'ᴾ', 'R' => 'ᴿ',
        'T' => 'ᵀ', 'U' => 'ᵁ', 'V' => 'ⱽ', 'W' => 'ᵂ',
        other => other,
    }
}

pub fn char_from_superscript(c: char) -> char {
    match c {
        '⁰' => '0', '¹' => '1', '²' => '2', '³' => '3', '⁴' => '4',
        '⁵' => '5', '⁶' => '6', '⁷' => '7', '⁸' => '8', '⁹' => '9',
        '⁺' => '+', '⁻' => '-', '⁼' => '=', '⁽' => '(', '⁾' => ')',
        'ᵃ' => 'a', 'ᵇ' => 'b', 'ᶜ' => 'c', 'ᵈ' => 'd', 'ᵉ' => 'e',
        'ᶠ' => 'f', 'ᵍ' => 'g', 'ʰ' => 'h', 'ⁱ' => 'i', 'ʲ' => 'j',
        'ᵏ' => 'k', 'ˡ' => 'l', 'ᵐ' => 'm', 'ⁿ' => 'n', 'ᵒ' => 'o',
        'ᵖ' => 'p', 'ʳ' => 'r', 'ˢ' => 's', 'ᵗ' => 't', 'ᵘ' => 'u',
        'ᵛ' => 'v', 'ʷ' => 'w', 'ˣ' => 'x', 'ʸ' => 'y', 'ᶻ' => 'z',
        'ᴬ' => 'A', 'ᴮ' => 'B', 'ᴰ' => 'D', 'ᴱ' => 'E', 'ᴳ' => 'G',
        'ᴴ' => 'H', 'ᴵ' => 'I', 'ᴶ' => 'J', 'ᴷ' => 'K', 'ᴸ' => 'L',
        'ᴹ' => 'M', 'ᴺ' => 'N', 'ᴼ' => 'O', 'ᴾ' => 'P', 'ᴿ' => 'R',
        'ᵀ' => 'T', 'ᵁ' => 'U', 'ⱽ' => 'V', 'ᵂ' => 'W',
        other => other,
    }
}

pub fn char_to_subscript(c: char) -> char {
    match c {
        '0' => '₀', '1' => '₁', '2' => '₂', '3' => '₃', '4' => '₄',
        '5' => '₅', '6' => '₆', '7' => '₇', '8' => '₈', '9' => '₉',
        '+' => '₊', '-' | '−' => '₋', '=' => '₌', '(' => '₍', ')' => '₎',
        'a' => 'ₐ', 'e' => 'ₑ', 'h' => 'ₕ', 'i' => 'ᵢ', 'j' => 'ⱼ',
        'k' => 'ₖ', 'l' => 'ₗ', 'm' => 'ₘ', 'n' => 'ₙ', 'o' => 'ₒ',
        'p' => 'ₚ', 'r' => 'ᵣ', 's' => 'ₛ', 't' => 'ₜ', 'u' => 'ᵤ',
        'v' => 'ᵥ', 'x' => 'ₓ',
        other => other,
    }
}

pub fn char_from_subscript(c: char) -> char {
    match c {
        '₀' => '0', '₁' => '1', '₂' => '2', '₃' => '3', '₄' => '4',
        '₅' => '5', '₆' => '6', '₇' => '7', '₈' => '8', '₉' => '9',
        '₊' => '+', '₋' => '-', '₌' => '=', '₍' => '(', '₎' => ')',
        'ₐ' => 'a', 'ₑ' => 'e', 'ₕ' => 'h', 'ᵢ' => 'i', 'ⱼ' => 'j',
        'ₖ' => 'k', 'ₗ' => 'l', 'ₘ' => 'm', 'ₙ' => 'n', 'ₒ' => 'o',
        'ₚ' => 'p', 'ᵣ' => 'r', 'ₛ' => 's', 'ₜ' => 't', 'ᵤ' => 'u',
        'ᵥ' => 'v', 'ₓ' => 'x',
        other => other,
    }
}

fn snap_char_boundaries(s: &str, start: usize, end: usize) -> (usize, usize) {
    let mut s_idx = start.min(s.len());
    while s_idx > 0 && !s.is_char_boundary(s_idx) {
        s_idx -= 1;
    }
    let mut e_idx = end.min(s.len());
    while e_idx < s.len() && !s.is_char_boundary(e_idx) {
        e_idx += 1;
    }
    (s_idx, e_idx)
}

pub fn toggle_superscript(ed: &mut LineEditor) -> bool {
    let (s_pos, e_pos) = if let Some(r) = &ed.selection {
        (r.start.min(ed.text.len()), r.end.min(ed.text.len()))
    } else {
        (0, ed.text.len())
    };
    let (start, end) = snap_char_boundaries(&ed.text, s_pos, e_pos);
    if start >= end {
        return false;
    }
    let target = &ed.text[start..end];
    let all_sup = target.chars().all(|c| char_from_superscript(c) != c);
    let converted: String = if all_sup {
        target.chars().map(char_from_superscript).collect()
    } else {
        target.chars().map(char_to_superscript).collect()
    };
    if converted != target {
        let conv_len = converted.len();
        ed.text.replace_range(start..end, &converted);
        if ed.selection.is_some() {
            ed.selection = Some(start..start + conv_len);
        }
        true
    } else {
        false
    }
}

pub fn toggle_subscript(ed: &mut LineEditor) -> bool {
    let (s_pos, e_pos) = if let Some(r) = &ed.selection {
        (r.start.min(ed.text.len()), r.end.min(ed.text.len()))
    } else {
        (0, ed.text.len())
    };
    let (start, end) = snap_char_boundaries(&ed.text, s_pos, e_pos);
    if start >= end {
        return false;
    }
    let target = &ed.text[start..end];
    let all_sub = target.chars().all(|c| char_from_subscript(c) != c);
    let converted: String = if all_sub {
        target.chars().map(char_from_subscript).collect()
    } else {
        target.chars().map(char_to_subscript).collect()
    };
    if converted != target {
        let conv_len = converted.len();
        ed.text.replace_range(start..end, &converted);
        if ed.selection.is_some() {
            ed.selection = Some(start..start + conv_len);
        }
        true
    } else {
        false
    }
}

/// Underline, value-based line spacing, paragraph spacing, character spacing and horizontal
/// scale for the paragraph being edited. Returns `true` when something changed.
pub(crate) fn extras_panel(ui: &mut egui::Ui, e: &mut Extras) -> bool {
    let before = *e;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("↕≡").strong()).on_hover_text(tl!("Line spacing"));
        let mut ls = if e.line_spacing <= 0.0 { 1.20 } else { e.line_spacing };
        if ui.add(egui::DragValue::new(&mut ls).range(0.5..=5.0).speed(0.01).custom_formatter(|v, _| format!("{v:.2}"))).changed() {
            e.line_spacing = (ls * 100.0).round() / 100.0;
        }
        egui::ComboBox::from_id_salt("line-spacing-presets").selected_text("").width(16.0).show_ui(ui, |ui| {
            for v in [1.0, 1.15, 1.25, 1.33, 1.5, 2.0, 2.5, 3.0] {
                if ui.selectable_label((e.line_spacing - v).abs() < 0.01, format!("{v:.2}×")).clicked() {
                    e.line_spacing = v;
                }
            }
        });
        ui.add_space(8.0);
        ui.label(egui::RichText::new("↕≡").weak()).on_hover_text(tl!("Paragraph spacing"));
        let mut ps = e.paragraph_spacing;
        if ui.add(egui::DragValue::new(&mut ps).range(0.0..=100.0).speed(0.05).custom_formatter(|v, _| format!("{v:.2}"))).changed() {
            e.paragraph_spacing = (ps * 100.0).round() / 100.0;
        }
        egui::ComboBox::from_id_salt("para-spacing-presets").selected_text("").width(16.0).show_ui(ui, |ui| {
            for v in [0.0, 0.13, 0.25, 0.5, 1.0, 2.0] {
                if ui.selectable_label((e.paragraph_spacing - v).abs() < 0.01, format!("{v:.2}")).clicked() {
                    e.paragraph_spacing = v;
                }
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("T↔").strong()).on_hover_text(tl!("Horizontal scale"));
        ui.add(egui::DragValue::new(&mut e.scale).range(10.0..=400.0).speed(1.0).custom_formatter(|v, _| format!("{v:.0}")));
        egui::ComboBox::from_id_salt("scale-presets").selected_text("").width(16.0).show_ui(ui, |ui| {
            for v in [50.0, 75.0, 90.0, 100.0, 110.0, 125.0, 150.0, 200.0] {
                if ui.selectable_label((e.scale - v).abs() < 0.01, format!("{v:.0}%")).clicked() {
                    e.scale = v;
                }
            }
        });
        ui.add_space(8.0);
        ui.label(egui::RichText::new("V↔A").strong()).on_hover_text(tl!("Character spacing"));
        ui.add(egui::DragValue::new(&mut e.char_spacing).range(-5.0..=50.0).speed(0.05).custom_formatter(|v, _| format!("{v:.2}")));
        egui::ComboBox::from_id_salt("char-spacing-presets").selected_text("").width(16.0).show_ui(ui, |ui| {
            for v in [-1.0, -0.5, -0.24, 0.0, 0.25, 0.5, 1.0, 2.0] {
                if ui.selectable_label((e.char_spacing - v).abs() < 0.01, format!("{v:.2} pt")).clicked() {
                    e.char_spacing = v;
                }
            }
        });
    });
    *e != before
}

impl LineEditor {
    pub(crate) fn has_changes(&self) -> bool {
        self.text != self.original || self.look != self.look0 || self.extras != self.extras0
    }

    /// Extra width permitted by the editor. Source containers now remain fixed.
    pub fn growth(&self) -> Option<f32> {
        (self.max_width > self.rect.width()).then_some(self.max_width)
    }

    /// Whether the edited text differs from the original (unsaved changes).
    pub fn has_unsaved_text(&self) -> bool {
        self.text != self.original
    }

    /// The formatting the panel changed.
    pub fn style(&self) -> pdfcraft_engine::BlockStyle {
        let (l, o) = (&self.look, &self.look0);
        let mut style = pdfcraft_engine::BlockStyle {
            resource: self.font_resource.clone(),
            family: (l.family != o.family || l.bold != o.bold || l.italic != o.italic).then_some((l.family, l.bold, l.italic)),
            size: (l.size != o.size).then_some(l.size),
            color: (l.color != o.color).then_some(l.color),
            align: (l.align != o.align).then_some(l.align),
            underline: (self.extras.underline != self.extras0.underline).then_some(self.extras.underline),
            line_spacing: (self.extras.line_spacing != self.extras0.line_spacing && self.extras.line_spacing > 0.0)
                .then_some(self.extras.line_spacing),
            char_spacing: (self.extras.char_spacing != self.extras0.char_spacing).then_some(self.extras.char_spacing),
            scale: (self.extras.scale != self.extras0.scale).then_some(self.extras.scale),
            ..Default::default()
        };
        if let Some(range) = &self.selection
            && (style.family.is_some()
                || style.resource.is_some()
                || style.size.is_some()
                || style.color.is_some()
                || style.underline.is_some()
                || style.char_spacing.is_some()
                || style.scale.is_some())
        {
            style.ranges.push(pdfcraft_engine::RangeStyle {
                range: range.clone(),
                family: style.family.take(),
                resource: style.resource.take(),
                size: style.size.take(),
                color: style.color.take(),
                underline: style.underline.take(),
                char_spacing: style.char_spacing.take(),
                scale: style.scale.take(),
            });
        }
        style
    }

    /// After applying formatting: the paragraph now has it.
    pub fn applied(&mut self) {
        self.selection = None;
        self.font_resource = None;
        self.look0 = self.look.clone();
        self.extras0 = self.extras;
        self.original = self.text.clone();
        self.focus = true;
    }

    /// Adopt the rewritten paragraph's current geometry before the next overlay frame.
    pub(crate) fn refresh_source(&mut self, block: &pdfcraft_engine::TextBlock, lines: Vec<pdfcraft_engine::TextLine>) {
        self.source_rect = block.rect.map(|v| v as f32);
        self.multiline = block.rows.len() > 1;
        self.source = Some((block.clone(), lines));
        self.text = block.editor_text.clone();
        self.original = self.text.clone();
    }
}

/// The look shown for a paragraph: the family and weight guessed from its PDF font name.
fn source_look(base_font: &str, size: f64, color: [f64; 3], detected_bold: bool, detected_italic: bool) -> pdfcraft_engine::AddedText {
    use pdfcraft_engine::FontFamily as F;
    let name = base_font.to_ascii_lowercase();
    let family = if ["courier", "mono", "consolas", "menlo", "monaco", "lucida console"].iter().any(|s| name.contains(s)) {
        F::Courier
    } else if !name.contains("sans") && ["times", "serif", "roman", "cambria", "georgia", "palatino", "garamond"].iter().any(|s| name.contains(s)) {
        F::Times
    } else {
        F::Helvetica
    };
    pdfcraft_engine::AddedText {
        family,
        bold: detected_bold || ["bold", "black", "heavy", "semibold", "demi"].iter().any(|s| name.contains(s)),
        italic: detected_italic || ["italic", "oblique", "slanted"].iter().any(|s| name.contains(s)),
        size: (size * 10.0).round() / 10.0,
        color,
        ..Default::default()
    }
}

fn look_of(b: &pdfcraft_engine::TextBlock) -> pdfcraft_engine::AddedText {
    source_look(&b.base_font, b.size, b.color, b.bold, b.italic)
}

fn editor_font(look: &pdfcraft_engine::AddedText, size: f32) -> FontId {
    let family = match (look.family, look.bold) {
        (pdfcraft_engine::FontFamily::Courier, _) => FontFamily::Monospace,
        (_, true) => FontFamily::Name("semibold".into()),
        (_, false) => FontFamily::Proportional,
    };
    FontId::new(size, family)
}

fn color32(color: [f64; 3]) -> Color32 {
    Color32::from_rgb(
        (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// The dark box behind text too pale to read on white.
const DARK_EDITOR_FILL: Color32 = Color32::from_gray(0x26);

/// WCAG relative luminance of an sRGB colour.
fn luminance(c: Color32) -> f32 {
    let channel = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

/// WCAG contrast ratio between two colours (1 to 21).
fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The inline editor's background for text drawn in `text`: white, or a dark box when the text
/// reads better on that (white or pale text, which would vanish on white).
pub(crate) fn editor_fill(text: Color32) -> Color32 {
    if contrast(text, Color32::WHITE) >= contrast(text, DARK_EDITOR_FILL) { Color32::WHITE } else { DARK_EDITOR_FILL }
}

/// The 8 resize handles on an image (4 corners + 4 edge centers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageHandle {
    TopLeft,
    TopCenter,
    TopRight,
    RightCenter,
    BottomRight,
    BottomCenter,
    BottomLeft,
    LeftCenter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragTarget {
    Move,
    Handle(ImageHandle),
    CropHandle(ImageHandle),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageDrag {
    pub start: Pos2,
    pub target: DragTarget,
    pub initial_rect: Rect,
}

/// A selected page image, and what the pointer is doing to it.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageSelection {
    pub page: usize,
    pub index: usize,
    pub crop_mode: bool,
    pub crop_rect: Option<Rect>,
    pub drag: Option<ImageDrag>,
}

impl ImageSelection {
    pub(crate) fn dragging(&self) -> bool {
        self.drag.is_some()
    }
}

/// Once a selected handle is grabbed, text under it cannot take over the gesture. The same
/// applies after the pointer leaves the image/page while dragging.
pub(crate) fn image_owns_pointer(ui: &egui::Ui, xf: &PageXform, page: usize, info: &DocInfo, view: &DocView) -> bool {
    let Some(selection) = view.image_selection.as_ref().filter(|s| s.page == page) else { return false };
    if selection.drag.is_some() {
        return true;
    }
    let Some(image) = view.edit_images.get(&page).and_then(|(_, images)| images.get(selection.index)) else { return false };
    let Some(rect) = image.visible_rect else { return false };
    let b = xf.user_rect(info, page, rect.map(|v| v as f32));
    let p = ui.input(|i| if i.pointer.primary_down() { i.pointer.press_origin() } else { i.pointer.hover_pos() });
    let handles = [
        b.left_top(),
        Pos2::new(b.center().x, b.top()),
        b.right_top(),
        Pos2::new(b.right(), b.center().y),
        b.right_bottom(),
        Pos2::new(b.center().x, b.bottom()),
        b.left_bottom(),
        Pos2::new(b.left(), b.center().y),
    ];
    p.is_some_and(|p| handles.iter().any(|c| c.distance(p) < 8.0))
}

/// What a drag on a paragraph box takes hold of.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grip {
    Move,
    /// An edge (screen left or right), dragged to rewrap the paragraph.
    Left,
    Right,
}

/// A paragraph box being dragged: moved, or (from a handle on its left or right edge) resized.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockDrag {
    page: usize,
    block: usize,
    /// A paragraph index belongs to the document generation where the press was resolved.
    generation: u64,
    /// Where the drag started (screen).
    start: Pos2,
    grip: Grip,
}

/// How far an edge grip reaches either side of a box's edge (screen pixels).
const EDGE_REACH: f32 = 6.0;

/// The rewrap handles drawn on a paragraph box's left and right edges (screen).
fn width_handles(b: Rect) -> [Rect; 2] {
    let h = (b.height() * 0.5).clamp(14.0, 28.0);
    [b.left(), b.right()].map(|x| Rect::from_center_size(Pos2::new(x, b.center().y), egui::vec2(7.0, h)))
}

/// The edge of `b` that `p` grabs: anywhere along its height, within reach of the line. A narrow
/// box keeps its middle for moving.
fn edge_at(b: Rect, p: Pos2) -> Option<Grip> {
    if p.y < b.top() - EDGE_REACH || p.y > b.bottom() + EDGE_REACH {
        return None;
    }
    let inside = EDGE_REACH.min(b.width() / 4.0);
    let (dl, dr) = (p.x - b.left(), b.right() - p.x);
    match (dl >= -EDGE_REACH && dl <= inside, dr >= -EDGE_REACH && dr <= inside) {
        (true, true) if dl <= dr => Some(Grip::Left),
        (_, true) => Some(Grip::Right),
        (true, false) => Some(Grip::Left),
        _ => None,
    }
}

/// The topmost box under `p` and what it grabs there; edges only on an upright page.
fn grab_at(boxes: &[Rect], p: Pos2, upright: bool) -> Option<(usize, Grip)> {
    boxes.iter().enumerate().rev().find_map(|(i, b)| match upright.then(|| edge_at(*b, p)).flatten() {
        Some(g) => Some((i, g)),
        None => b.contains(p).then_some((i, Grip::Move)),
    })
}

/// What a right-click on a selected image asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ImageAction {
    Replace(usize, usize),
    Save(usize, usize),
    Copy(usize, usize),
    CopyFile(usize, usize),
    Cut(usize, usize),
    DrawFreehand,
    Redact(usize, usize),
    CreateLink(usize, usize),
    RecognizeText,
    AddComment(usize, usize),
    Crop,
}

fn user_box(xf: &PageXform, info: &DocInfo, page: usize, r: Rect) -> [f64; 4] {
    let p = &info.pages[page];
    let (a, b) = (xf.screen_to_view(r.min), xf.screen_to_view(r.max));
    let (u, v) = (p.view_to_user(a.0, a.1), p.view_to_user(b.0, b.1));
    [u[0].min(v[0]) as f64, u[1].min(v[1]) as f64, u[0].max(v[0]) as f64, u[1].max(v[1]) as f64]
}

/// Images on a page: select, move, resize, right-click. Returns `true` when the pointer was used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn image_input(
    ui: &egui::Ui,
    resp: &egui::Response,
    xf: &PageXform,
    page: usize,
    info: &DocInfo,
    images: &[pdfcraft_engine::PageImage],
    view: &mut DocView,
    _action: &mut Option<ImageAction>,
    allow_interaction: bool,
) -> bool {
    let boxes: Vec<Rect> = images.iter().map(|im| im.visible_rect.map_or(Rect::NOTHING, |r| xf.user_rect(info, page, r.map(|v| v as f32)))).collect();
    let painter = ui.painter();
    let hover_pos = ui.input(|i| i.pointer.hover_pos());
    let image_contains = |index: usize, point: Pos2| {
        let (x, y) = xf.screen_to_view(point);
        let Some(p) = info.pages.get(page) else { return false };
        let user = p.view_to_user(x, y);
        images.get(index).is_some_and(|im| im.contains(user.map(f64::from)))
    };
    let selected = view.image_selection.as_ref().filter(|s| s.page == page).map(|s| s.index).filter(|i| *i < boxes.len());
    let dragging = view.image_selection.as_ref().is_some_and(|s| s.drag.is_some());
    // Few images: faint outlines for all of them, as before. Many: only the selected and
    // hovered boxes, so a ~1000-image CAD page stays interactive in Edit text & images.
    // Fully clipped placements (empty visible frames) get no outline.
    let faint_all = boxes.len() <= MAX_OUTLINES;
    if faint_all {
        for b in boxes.iter().filter(|b| b.is_positive()) {
            painter.rect_stroke(*b, CornerRadius::ZERO, Stroke::new(0.75, ACCENT.gamma_multiply(0.35)), egui::StrokeKind::Outside);
        }
    }
    let hovered_box = hover_pos.and_then(|p| boxes.iter().enumerate().rposition(|(i, b)| b.is_positive() && b.contains(p) && image_contains(i, p)));
    if !faint_all {
        for i in selected.into_iter().chain(hovered_box.filter(|h| Some(*h) != selected)) {
            if let Some(b) = boxes.get(i).filter(|b| b.is_positive()) {
                painter.rect_stroke(*b, CornerRadius::ZERO, Stroke::new(0.75, ACCENT.gamma_multiply(0.35)), egui::StrokeKind::Outside);
            }
        }
    }
    for i in selected.into_iter().chain(hovered_box).collect::<Vec<_>>() {
        let Some(b) = boxes.get(i).copied().filter(|b| b.is_positive()) else { continue };
        if selected == Some(i) {
            // The selected frame and handles are drawn below (or the drag preview instead).
            if !dragging {
                painter.rect_filled(b, CornerRadius::ZERO, ACCENT.gamma_multiply(0.04));
            }
        } else {
            painter.rect_stroke(b, CornerRadius::ZERO, Stroke::new(1.5, ACCENT.gamma_multiply(0.7)), egui::StrokeKind::Outside);
        }
    }
    // The selected image: frame, 8 handles (or crop mode), dragging.
    if let Some(i) = selected.filter(|i| boxes.get(*i).is_some_and(|b| b.is_positive())) {
        let b = boxes[i];
        let is_crop = view.image_selection.as_ref().is_some_and(|s| s.crop_mode);

        let handle_pos = |rect: Rect, h: ImageHandle| match h {
            ImageHandle::TopLeft => rect.left_top(),
            ImageHandle::TopCenter => Pos2::new(rect.center().x, rect.top()),
            ImageHandle::TopRight => rect.right_top(),
            ImageHandle::RightCenter => Pos2::new(rect.right(), rect.center().y),
            ImageHandle::BottomRight => rect.right_bottom(),
            ImageHandle::BottomCenter => Pos2::new(rect.center().x, rect.bottom()),
            ImageHandle::BottomLeft => rect.left_bottom(),
            ImageHandle::LeftCenter => Pos2::new(rect.left(), rect.center().y),
        };
        let all_handles = [
            ImageHandle::TopLeft,
            ImageHandle::TopCenter,
            ImageHandle::TopRight,
            ImageHandle::RightCenter,
            ImageHandle::BottomRight,
            ImageHandle::BottomCenter,
            ImageHandle::BottomLeft,
            ImageHandle::LeftCenter,
        ];
        let cursor_for = |h: ImageHandle| match h {
            ImageHandle::TopLeft | ImageHandle::BottomRight => egui::CursorIcon::ResizeNwSe,
            ImageHandle::TopRight | ImageHandle::BottomLeft => egui::CursorIcon::ResizeNeSw,
            ImageHandle::TopCenter | ImageHandle::BottomCenter => egui::CursorIcon::ResizeVertical,
            ImageHandle::LeftCenter | ImageHandle::RightCenter => egui::CursorIcon::ResizeHorizontal,
        };
        let draw_handles = |rect: Rect| {
            for &h in &all_handles {
                let c = handle_pos(rect, h);
                match h {
                    ImageHandle::TopLeft | ImageHandle::TopRight | ImageHandle::BottomLeft | ImageHandle::BottomRight => {
                        painter.rect(
                            Rect::from_center_size(c, egui::vec2(8.0, 8.0)),
                            CornerRadius::ZERO,
                            Color32::WHITE,
                            Stroke::new(1.0, ACCENT),
                            egui::StrokeKind::Middle,
                        );
                    }
                    ImageHandle::TopCenter | ImageHandle::BottomCenter => {
                        painter.rect(
                            Rect::from_center_size(c, egui::vec2(12.0, 6.0)),
                            CornerRadius::same(1),
                            Color32::WHITE,
                            Stroke::new(1.0, ACCENT),
                            egui::StrokeKind::Middle,
                        );
                    }
                    ImageHandle::LeftCenter | ImageHandle::RightCenter => {
                        painter.rect(
                            Rect::from_center_size(c, egui::vec2(6.0, 12.0)),
                            CornerRadius::same(1),
                            Color32::WHITE,
                            Stroke::new(1.0, ACCENT),
                            egui::StrokeKind::Middle,
                        );
                    }
                }
            }
        };

        if is_crop {
            let active_crop = view.image_selection.as_ref().and_then(|s| s.crop_rect).unwrap_or(b);
            let shade_color = Color32::from_rgba_unmultiplied(15, 35, 70, 150);
            if active_crop.top() > b.top() {
                painter.rect_filled(Rect::from_min_max(b.min, Pos2::new(b.right(), active_crop.top())), CornerRadius::ZERO, shade_color);
            }
            if active_crop.bottom() < b.bottom() {
                painter.rect_filled(Rect::from_min_max(Pos2::new(b.left(), active_crop.bottom()), b.max), CornerRadius::ZERO, shade_color);
            }
            if active_crop.left() > b.left() {
                painter.rect_filled(Rect::from_min_max(Pos2::new(b.left(), active_crop.top()), Pos2::new(active_crop.left(), active_crop.bottom())), CornerRadius::ZERO, shade_color);
            }
            if active_crop.right() < b.right() {
                painter.rect_filled(Rect::from_min_max(Pos2::new(active_crop.right(), active_crop.top()), Pos2::new(b.right(), active_crop.bottom())), CornerRadius::ZERO, shade_color);
            }

            painter.rect_stroke(active_crop, CornerRadius::ZERO, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
            let x1 = active_crop.left() + active_crop.width() / 3.0;
            let x2 = active_crop.left() + active_crop.width() * 2.0 / 3.0;
            let y1 = active_crop.top() + active_crop.height() / 3.0;
            let y2 = active_crop.top() + active_crop.height() * 2.0 / 3.0;
            painter.line_segment([Pos2::new(x1, active_crop.top()), Pos2::new(x1, active_crop.bottom())], Stroke::new(0.75, ACCENT.gamma_multiply(0.5)));
            painter.line_segment([Pos2::new(x2, active_crop.top()), Pos2::new(x2, active_crop.bottom())], Stroke::new(0.75, ACCENT.gamma_multiply(0.5)));
            painter.line_segment([Pos2::new(active_crop.left(), y1), Pos2::new(active_crop.right(), y1)], Stroke::new(0.75, ACCENT.gamma_multiply(0.5)));
            painter.line_segment([Pos2::new(active_crop.left(), y2), Pos2::new(active_crop.right(), y2)], Stroke::new(0.75, ACCENT.gamma_multiply(0.5)));

            let crop_handles = [
                ImageHandle::TopCenter,
                ImageHandle::RightCenter,
                ImageHandle::BottomCenter,
                ImageHandle::LeftCenter,
            ];
            for &h in &crop_handles {
                let c = handle_pos(active_crop, h);
                let sz = if matches!(h, ImageHandle::TopCenter | ImageHandle::BottomCenter) { egui::vec2(16.0, 7.0) } else { egui::vec2(7.0, 16.0) };
                painter.rect(Rect::from_center_size(c, sz), CornerRadius::same(1), Color32::WHITE, Stroke::new(1.5, ACCENT), egui::StrokeKind::Middle);
            }

            if allow_interaction && let Some(p) = hover_pos {
                if let Some(&h) = crop_handles.iter().find(|&&h| handle_pos(active_crop, h).distance(p) < 8.0) {
                    ui.ctx().set_cursor_icon(cursor_for(h));
                } else if active_crop.contains(p) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
                }
            }

            if allow_interaction {
                if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                    if active_crop != b {
                        view.pending_edit = Some(Edit::EditPageImage {
                            page,
                            index: i,
                            change: pdfcraft_engine::ImageEdit::Crop(user_box(xf, info, page, active_crop)),
                        });
                    }
                    if let Some(s) = view.image_selection.as_mut() {
                        s.crop_mode = false;
                        s.crop_rect = None;
                    }
                    return true;
                }
                if ui.input(|inp| inp.key_pressed(egui::Key::Escape)) {
                    if let Some(s) = view.image_selection.as_mut() {
                        s.crop_mode = false;
                        s.crop_rect = None;
                    }
                    return true;
                }
            }
        } else {
            if view.image_selection.as_ref().is_some_and(|s| s.drag.is_none()) {
                painter.rect_stroke(b, CornerRadius::ZERO, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
                draw_handles(b);
            }
            if allow_interaction
                && let Some(p) = hover_pos
                && let Some(&h) = all_handles.iter().find(|&&h| handle_pos(b, h).distance(p) < 8.0)
            {
                ui.ctx().set_cursor_icon(cursor_for(h));
            }
        }

        let origin = ui.input(|i| i.pointer.press_origin());
        if allow_interaction && resp.drag_started() && let Some(o) = origin {
            if is_crop {
                let active_crop = view.image_selection.as_ref().and_then(|s| s.crop_rect).unwrap_or(b);
                let crop_handles = [
                    ImageHandle::TopCenter,
                    ImageHandle::RightCenter,
                    ImageHandle::BottomCenter,
                    ImageHandle::LeftCenter,
                ];
                let hit_edge = crop_handles.iter().copied().find(|&h| handle_pos(active_crop, h).distance(o) < 8.0);
                if let Some(h) = hit_edge {
                    if let Some(s) = view.image_selection.as_mut() {
                        s.drag = Some(ImageDrag { start: o, target: DragTarget::CropHandle(h), initial_rect: active_crop });
                    }
                } else if active_crop.contains(o)
                    && let Some(s) = view.image_selection.as_mut()
                {
                    s.drag = Some(ImageDrag { start: o, target: DragTarget::Move, initial_rect: active_crop });
                }
            } else {
                let hit_handle = all_handles.iter().copied().find(|&h| handle_pos(b, h).distance(o) < 8.0);
                if let Some(h) = hit_handle {
                    if let Some(s) = view.image_selection.as_mut() {
                        s.drag = Some(ImageDrag { start: o, target: DragTarget::Handle(h), initial_rect: b });
                    }
                } else if image_contains(i, o)
                    && let Some(s) = view.image_selection.as_mut()
                {
                    s.drag = Some(ImageDrag { start: o, target: DragTarget::Move, initial_rect: b });
                }
            }
        }

        if let Some(drag) = view.image_selection.as_ref().and_then(|s| s.drag)
            && let Some(p) = hover_pos
        {
            let delta = p - drag.start;
            let initial = drag.initial_rect;
            if is_crop {
                let mut new_crop = initial;
                match drag.target {
                    DragTarget::CropHandle(ImageHandle::TopCenter) => {
                        let y = (initial.top() + delta.y).clamp(b.top(), initial.bottom() - 10.0);
                        new_crop = Rect::from_min_max(Pos2::new(initial.left(), y), initial.max);
                    }
                    DragTarget::CropHandle(ImageHandle::BottomCenter) => {
                        let y = (initial.bottom() + delta.y).clamp(initial.top() + 10.0, b.bottom());
                        new_crop = Rect::from_min_max(initial.min, Pos2::new(initial.right(), y));
                    }
                    DragTarget::CropHandle(ImageHandle::LeftCenter) => {
                        let x = (initial.left() + delta.x).clamp(b.left(), initial.right() - 10.0);
                        new_crop = Rect::from_min_max(Pos2::new(x, initial.top()), initial.max);
                    }
                    DragTarget::CropHandle(ImageHandle::RightCenter) => {
                        let x = (initial.right() + delta.x).clamp(initial.left() + 10.0, b.right());
                        new_crop = Rect::from_min_max(initial.min, Pos2::new(x, initial.bottom()));
                    }
                    DragTarget::Move => {
                        let mut translated = initial.translate(delta);
                        if translated.left() < b.left() {
                            translated = translated.translate(egui::vec2(b.left() - translated.left(), 0.0));
                        }
                        if translated.right() > b.right() {
                            translated = translated.translate(egui::vec2(b.right() - translated.right(), 0.0));
                        }
                        if translated.top() < b.top() {
                            translated = translated.translate(egui::vec2(0.0, b.top() - translated.top()));
                        }
                        if translated.bottom() > b.bottom() {
                            translated = translated.translate(egui::vec2(0.0, b.bottom() - translated.bottom()));
                        }
                        new_crop = translated;
                    }
                    _ => {}
                }
                if let Some(s) = view.image_selection.as_mut() {
                    s.crop_rect = Some(new_crop);
                }
                if (resp.drag_stopped() || !ui.input(|i| i.pointer.primary_down()))
                    && let Some(s) = view.image_selection.as_mut()
                {
                    s.drag = None;
                }
                ui.ctx().set_cursor_icon(match drag.target {
                    DragTarget::CropHandle(h) => cursor_for(h),
                    _ => egui::CursorIcon::Grabbing,
                });
                return true;
            } else {
                let preview = match drag.target {
                    DragTarget::Move => initial.translate(delta),
                    DragTarget::Handle(ImageHandle::TopCenter) => {
                        let new_top = (initial.top() + delta.y).min(initial.bottom() - 4.0);
                        Rect::from_min_max(Pos2::new(initial.left(), new_top), Pos2::new(initial.right(), initial.bottom()))
                    }
                    DragTarget::Handle(ImageHandle::BottomCenter) => {
                        let new_bottom = (initial.bottom() + delta.y).max(initial.top() + 4.0);
                        Rect::from_min_max(Pos2::new(initial.left(), initial.top()), Pos2::new(initial.right(), new_bottom))
                    }
                    DragTarget::Handle(ImageHandle::LeftCenter) => {
                        let new_left = (initial.left() + delta.x).min(initial.right() - 4.0);
                        Rect::from_min_max(Pos2::new(new_left, initial.top()), Pos2::new(initial.right(), initial.bottom()))
                    }
                    DragTarget::Handle(ImageHandle::RightCenter) => {
                        let new_right = (initial.right() + delta.x).max(initial.left() + 4.0);
                        Rect::from_min_max(Pos2::new(initial.left(), initial.top()), Pos2::new(new_right, initial.bottom()))
                    }
                    DragTarget::Handle(h @ (ImageHandle::TopLeft | ImageHandle::TopRight | ImageHandle::BottomLeft | ImageHandle::BottomRight)) => {
                        let fixed = match h {
                            ImageHandle::TopLeft => Pos2::new(initial.right(), initial.bottom()),
                            ImageHandle::TopRight => Pos2::new(initial.left(), initial.bottom()),
                            ImageHandle::BottomLeft => Pos2::new(initial.right(), initial.top()),
                            ImageHandle::BottomRight => Pos2::new(initial.left(), initial.top()),
                            _ => Pos2::ZERO,
                        };
                        let start = drag.start;
                        let (w0, h0) = (initial.width().max(1.0), initial.height().max(1.0));
                        let horizontal = (p.x - start.x) * (start.x - fixed.x).signum() / w0;
                        let vertical = (p.y - start.y) * (start.y - fixed.y).signum() / h0;
                        let delta = if horizontal.abs() > vertical.abs() { horizontal } else { vertical };
                        let k = (1.0 + delta).max(0.05);
                        let (w, h) = (w0 * k, h0 * k);
                        let x = if start.x < fixed.x { fixed.x - w } else { fixed.x };
                        let y = if start.y < fixed.y { fixed.y - h } else { fixed.y };
                        Rect::from_min_size(Pos2::new(x, y), egui::vec2(w, h))
                    }
                    _ => initial,
                };
                let raw = images.get(i).map_or(b, |im| xf.user_rect(info, page, im.rect.map(|v| v as f32)));
                let ratio = |new: f32, old: f32| if old.abs() > 1e-3 && (new / old).is_finite() { new / old } else { 1.0 };
                let factor = egui::vec2(ratio(preview.width(), b.width()), ratio(preview.height(), b.height()));
                let target = Rect::from_min_max(preview.min + (raw.min - b.min) * factor, preview.min + (raw.max - b.min) * factor);
                let visible = preview.intersect(xf.rect);
                if visible.is_positive() {
                    painter.rect_filled(visible, CornerRadius::ZERO, ACCENT.gamma_multiply(0.04));
                    painter.rect_stroke(visible, CornerRadius::ZERO, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
                    draw_handles(preview);
                }
                if resp.drag_stopped() || !ui.input(|i| i.pointer.primary_down()) {
                    if let Some(s) = view.image_selection.as_mut() {
                        s.drag = None;
                    }
                    if preview != b {
                        view.pending_edit =
                            Some(Edit::EditPageImage { page, index: i, change: pdfcraft_engine::ImageEdit::Move(user_box(xf, info, page, target)) });
                    }
                }
                ui.ctx().set_cursor_icon(match drag.target {
                    DragTarget::Handle(h) => cursor_for(h),
                    _ => egui::CursorIcon::Grabbing,
                });
                return true;
            }
        }
        if allow_interaction
            && ui.input(|inp| inp.key_pressed(egui::Key::Delete) || inp.key_pressed(egui::Key::Backspace))
            && !ui.ctx().egui_wants_keyboard_input()
        {
            view.image_selection = None;
            view.pending_edit = Some(Edit::EditPageImage { page, index: i, change: pdfcraft_engine::ImageEdit::Delete });
            return true;
        }
    }
    if !allow_interaction {
        return false;
    }
    let Some(p) = hover_pos.filter(|p| xf.rect.contains(*p)) else { return false };
    let Some(hit) = boxes.iter().enumerate().rposition(|(i, b)| b.contains(p) && image_contains(i, p)) else {
        let over_popup = egui::Popup::is_any_open(ui.ctx())
            || ui.ctx().is_pointer_over_egui()
            || ui.ctx().egui_wants_pointer_input();
        if (resp.secondary_clicked() || resp.clicked()) && !over_popup {
            if let Some(s) = view.image_selection.as_mut() && s.crop_mode {
                let img_box = boxes.get(s.index).copied();
                if let Some(crop) = s.crop_rect.filter(|c| img_box.is_some_and(|b| *c != b)) {
                    view.pending_edit = Some(Edit::EditPageImage {
                        page,
                        index: s.index,
                        change: pdfcraft_engine::ImageEdit::Crop(user_box(xf, info, page, crop)),
                    });
                }
            }
            view.image_selection = None;
        }
        return false;
    };
    if selected != Some(hit) {
        painter.rect_stroke(boxes[hit], CornerRadius::ZERO, Stroke::new(1.5, ACCENT.gamma_multiply(0.7)), egui::StrokeKind::Outside);
    }
    let Some(b) = boxes.get(hit).copied() else { return false };
    let corner = [b.left_top(), b.right_top(), b.left_bottom(), b.right_bottom()].iter().position(|c| c.distance(p) < 8.0);
    ui.ctx().set_cursor_icon(match (selected == Some(hit), corner) {
        (true, Some(0 | 3)) => egui::CursorIcon::ResizeNwSe,
        (true, Some(_)) => egui::CursorIcon::ResizeNeSw,
        (true, None) => egui::CursorIcon::Move,
        _ => egui::CursorIcon::PointingHand,
    });
    if resp.clicked() || resp.secondary_clicked() {
        view.image_selection = Some(ImageSelection { page, index: hit, crop_mode: false, crop_rect: None, drag: None });
    }
    true
}

/// The right-click context menu for a selected image matching Adobe Acrobat.
pub(crate) fn image_context_menu(
    ui: &mut egui::Ui,
    view: &mut DocView,
    doc: &pdfcraft_engine::Document,
    page: usize,
    hit: usize,
    action: &mut Option<ImageAction>,
) {
    let images = doc.page_images(page);
    let Some(img) = images.get(hit) else { return };
    use pdfcraft_engine::ImageEdit as E;

    // Top horizontal icon bar: Comment, Crop, Rotate CCW, Rotate CW, Delete
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if crate::icons::button(ui, "message-square", 24.0, false, tl!("Add a sticky note here")).clicked() {
            *action = Some(ImageAction::AddComment(page, hit));
            ui.close();
        }
        if crate::icons::button(ui, "crop", 24.0, false, tl!("Crop")).clicked() {
            *action = Some(ImageAction::Crop);
            if let Some(s) = view.image_selection.as_mut() {
                s.crop_mode = true;
                s.crop_rect = None;
            }
            ui.close();
        }
        if crate::icons::button(ui, "rotate-ccw", 24.0, false, tl!("Rotate Counterclockwise")).clicked() {
            view.pending_edit = Some(Edit::EditPageImage { page, index: hit, change: E::Rotate(3) });
            ui.close();
        }
        if crate::icons::button(ui, "rotate-cw", 24.0, false, tl!("Rotate Clockwise")).clicked() {
            view.pending_edit = Some(Edit::EditPageImage { page, index: hit, change: E::Rotate(1) });
            ui.close();
        }
        if crate::icons::button(ui, "trash-2", 24.0, false, tl!("Delete")).clicked() {
            view.image_selection = None;
            view.pending_edit = Some(Edit::EditPageImage { page, index: hit, change: E::Delete });
            ui.close();
        }
    });

    ui.separator();

    let raster = !img.is_form;
    if ui.add(egui::Button::new(tl!("Cut")).shortcut_text("Ctrl+X")).clicked() {
        *action = Some(ImageAction::Cut(page, hit));
        ui.close();
    }
    if ui.add(egui::Button::new(tl!("Copy")).shortcut_text("Ctrl+C")).clicked() {
        *action = Some(ImageAction::Copy(page, hit));
        ui.close();
    }
    // Paste item: disabled when no item can be pasted into the image layer
    ui.add_enabled(false, egui::Button::new(tl!("Paste")).shortcut_text("Ctrl+V"));

    ui.separator();

    let is_cropped = img.visible_rect.is_some() && img.visible_rect != Some(img.rect);
    if ui.add_enabled(is_cropped, egui::Button::new(tl!("Remove crop"))).clicked() {
        view.pending_edit = Some(Edit::EditPageImage { page, index: hit, change: E::RemoveCrop });
        ui.close();
    }

    if ui.button(tl!("Flip horizontal")).clicked() {
        view.pending_edit = Some(Edit::EditPageImage { page, index: hit, change: E::Flip { horizontal: true } });
        ui.close();
    }
    if ui.button(tl!("Flip vertical")).clicked() {
        view.pending_edit = Some(Edit::EditPageImage { page, index: hit, change: E::Flip { horizontal: false } });
        ui.close();
    }

    ui.menu_button(tl!("Arrange image"), |ui| {
        let total = images.len();
        if ui.button(tl!("Bring to front")).clicked() {
            view.pending_edit = Some(Edit::EditPageImage {
                page,
                index: hit,
                change: E::Arrange(pdfcraft_engine::ArrangeOrder::BringToFront),
            });
            if let Some(s) = view.image_selection.as_mut() {
                s.index = total.saturating_sub(1);
            }
            ui.close();
        }
        if ui.button(tl!("Send to back")).clicked() {
            view.pending_edit = Some(Edit::EditPageImage {
                page,
                index: hit,
                change: E::Arrange(pdfcraft_engine::ArrangeOrder::SendToBack),
            });
            if let Some(s) = view.image_selection.as_mut() {
                s.index = 0;
            }
            ui.close();
        }
        if ui.button(tl!("Bring forward")).clicked() {
            view.pending_edit = Some(Edit::EditPageImage {
                page,
                index: hit,
                change: E::Arrange(pdfcraft_engine::ArrangeOrder::BringForward),
            });
            if let Some(s) = view.image_selection.as_mut() {
                s.index = (hit + 1).min(total.saturating_sub(1));
            }
            ui.close();
        }
        if ui.button(tl!("Send backward")).clicked() {
            view.pending_edit = Some(Edit::EditPageImage {
                page,
                index: hit,
                change: E::Arrange(pdfcraft_engine::ArrangeOrder::SendBackward),
            });
            if let Some(s) = view.image_selection.as_mut() {
                s.index = hit.saturating_sub(1);
            }
            ui.close();
        }
    });

    if ui.add_enabled(raster, egui::Button::new(tl!("Replace image"))).clicked() {
        *action = Some(ImageAction::Replace(page, hit));
        ui.close();
    }

    ui.separator();

    if ui.button(tl!("Draw freehand")).clicked() {
        *action = Some(ImageAction::DrawFreehand);
        ui.close();
    }
    if ui.button(tl!("Redact image")).clicked() {
        *action = Some(ImageAction::Redact(page, hit));
        ui.close();
    }
    if ui.add_enabled(raster, egui::Button::new(tl!("Export image"))).clicked() {
        *action = Some(ImageAction::Save(page, hit));
        ui.close();
    }

    ui.separator();

    if ui.button(tl!("Create link")).clicked() {
        *action = Some(ImageAction::CreateLink(page, hit));
        ui.close();
    }

    ui.menu_button(tl!("Edit image using"), |ui| {
        ui.add_enabled(false, egui::Button::new(tl!("Default image editor")));
    });

    if ui.button(tl!("Recognize text")).clicked() {
        *action = Some(ImageAction::RecognizeText);
        ui.close();
    }
}

/// Lines and their screen boxes for a page; hover outlines, click opens the editor. Returns
/// `true` when the pointer was used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page_input(
    ui: &egui::Ui,
    resp: &egui::Response,
    xf: &PageXform,
    page: usize,
    doc: &pdfcraft_engine::Document,
    lines: &[pdfcraft_engine::TextBlock],
    view: &mut DocView,
    allow_interaction: bool,
) -> bool {
    let info = &doc.info;
    if view.block_drag.is_some_and(|d| d.generation != doc.edit_generation()) {
        view.block_drag = None;
        return true;
    }
    let boxes: Vec<Rect> = lines.iter().map(|l| xf.user_rect(info, page, l.rect.map(|v| v as f32)).expand(2.0)).collect();
    let painter = ui.painter();
    let active = view.line_editor.as_ref().filter(|e| e.page == page).map(|e| e.block);
    for (i, b) in boxes.iter().enumerate() {
        if active != Some(i) {
            painter.rect_stroke(*b, CornerRadius::same(2), Stroke::new(0.75, ACCENT.gamma_multiply(0.35)), egui::StrokeKind::Outside);
        }
    }
    if !allow_interaction {
        return false;
    }
    // The width handle needs the page upright (or upside down): on a quarter-turned page the
    // screen's horizontal is the paragraph's vertical.
    let upright = info.pages.get(page).is_some_and(|p| p.rotation % 180 == 0);
    let draw_handles = |b: Rect| {
        for h in width_handles(b) {
            painter.rect(h, CornerRadius::same(2), Color32::WHITE, Stroke::new(1.0, ACCENT), egui::StrokeKind::Middle);
        }
    };
    // A drag in progress: the box follows the pointer (or the grabbed edge does); releasing applies it.
    if let Some(d) = view.block_drag.filter(|d| d.page == page)
        && let (Some(b), Some(l)) = (boxes.get(d.block).copied(), lines.get(d.block))
    {
        let p = ui.input(|i| i.pointer.interact_pos()).unwrap_or(d.start);
        let dx = p.x - d.start.x;
        let preview = match d.grip {
            Grip::Move => b.translate(p - d.start),
            Grip::Left => Rect::from_min_max(Pos2::new((b.left() + dx).min(b.right() - 12.0), b.min.y), b.max),
            Grip::Right => Rect::from_min_max(b.min, Pos2::new((b.right() + dx).max(b.left() + 12.0), b.max.y)),
        };
        painter.rect_stroke(preview, CornerRadius::same(2), Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
        let resizing = d.grip != Grip::Move;
        if resizing {
            draw_handles(preview);
        }
        ui.ctx().set_cursor_icon(if resizing { egui::CursorIcon::ResizeHorizontal } else { egui::CursorIcon::Grabbing });
        if resp.drag_stopped() || !ui.input(|i| i.pointer.any_down()) {
            view.block_drag = None;
            let (from, to) = (user_box(xf, info, page, b), user_box(xf, info, page, preview));
            let edit = if resizing {
                // The new width in user space: the box's change, added to the paragraph's own. A
                // moved left side (in user space; on an upside-down page that's the screen's right
                // edge) moves the paragraph with it.
                let width = (l.rect[2] - l.rect[0]) + (to[2] - to[0]) - (from[2] - from[0]);
                let shift = to[0] - from[0];
                let style = pdfcraft_engine::BlockStyle { width: Some(width), offset: (shift != 0.0).then_some([shift, 0.0]), ..Default::default() };
                Edit::EditTextBlock { page, block: d.block, text: l.text.clone(), style }
            } else {
                // Translation must keep the selected source operators. Rewriting unchanged text
                // would also remove unrelated paragraphs that now overlap this one (#594).
                Edit::MoveObjects {
                    page,
                    objects: vec![ObjectTarget { kind: ObjectKind::Text, index: d.block }],
                    offset: [to[0] - from[0], to[1] - from[1]],
                }
            };
            if preview != b {
                view.pending_edit = Some(edit);
            }
        }
        return true;
    }
    // Dragging a box (not while a paragraph is open for typing) moves it; from an edge, resizes it.
    // What it grabs comes from where the button went down, not where the pointer is now: egui
    // only calls it a drag once the pointer has moved a few pixels (or been held a moment), and a
    // quick flick has left the edge by then.
    if resp.drag_started()
        && active.is_none()
        && let Some(o) = ui.input(|i| i.pointer.press_origin())
        && let Some((block, grip)) = grab_at(&boxes, o, upright)
    {
        view.block_drag = Some(BlockDrag { page, block, generation: doc.edit_generation(), start: o, grip });
        return true;
    }
    let Some(p) = ui.input(|i| i.pointer.hover_pos()).filter(|p| xf.rect.contains(*p)) else { return false };
    let Some((hit, grip)) = grab_at(&boxes, p, upright) else { return false };
    if active != Some(hit) {
        painter.rect_stroke(boxes[hit], CornerRadius::same(2), Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
    }
    let on_edge = active.is_none() && grip != Grip::Move;
    if upright && active.is_none() {
        draw_handles(boxes[hit]);
    }
    ui.ctx().set_cursor_icon(if on_edge { egui::CursorIcon::ResizeHorizontal } else { egui::CursorIcon::Text });
    if resp.clicked() {
        view.image_selection = None;
        let l = &lines[hit];
        let source_lines = doc.text_lines(page);
        let text = l.editor_text.clone();
        // Screen pixels per point, from the box's width.
        let scale = (boxes[hit].width() - 4.0) / ((l.rect[2] - l.rect[0]).max(1.0) as f32);
        // How far the editor box may grow: a single-line paragraph's rewrite grows to the
        // page's right edge, a multi-line paragraph rewraps to its own width.
        let right = xf.rect.right().min(view.viewport_rect().right()) - 6.0;
        let max_width = if l.rows.len() == 1 { (right - boxes[hit].left()).max(boxes[hit].width()) } else { boxes[hit].width() };
        view.line_editor = Some(LineEditor {
            selection: None,
            font_resource: None,
            page,
            block: hit,
            text: text.clone(),
            original: text,
            rect: boxes[hit],
            source_rect: l.rect.map(|v| v as f32),
            multiline: l.rows.len() > 1,
            source: Some((l.clone(), source_lines)),
            fonts: install_source_fonts(ui.ctx(), doc, page),
            overflow: false,
            max_width,
            size: (l.size as f32 * scale).clamp(8.0, 72.0),
            focus: true,
            look: look_of(l),
            look0: look_of(l),
            extras: Extras::default(),
            extras0: Extras::default(),
        });
    }
    true
}

/// One TextEdit lays out all source runs; its caret and selection span the whole region.
pub(crate) fn install_source_fonts(ctx: &egui::Context, doc: &pdfcraft_engine::Document, page: usize) -> BTreeMap<String, String> {
    let programs = doc.text_editor_fonts(page);
    let mut definitions = ctx.fonts(|f| f.definitions().clone());
    // Only the current editor's document fonts live in the atlas. Never accumulate the
    // programs of previously opened files or add them to the interface's default families.
    definitions.font_data.retain(|name, _| !name.starts_with("pdf-source-"));
    definitions.families.retain(|family, _| !matches!(family, FontFamily::Name(name) if name.starts_with("pdf-source-")));
    let mut families = BTreeMap::new();
    for (index, program) in programs.iter().enumerate() {
        let name = format!("pdf-source-{index}");
        definitions.font_data.insert(name.clone(), Arc::new(egui::FontData::from_owned(program.bytes.clone())));
        families.insert(program.resource.clone(), name);
    }
    for program in &programs {
        let Some(name) = families.get(&program.resource) else { continue };
        // Word-created PDFs often split one face into several subsets. Use all of the
        // document's matching subsets before falling back for newly inserted characters.
        let mut stack = vec![name.clone()];
        stack.extend(
            programs
                .iter()
                .filter(|p| {
                    pdfcraft_fonts::embedding::face_key(&p.face) == pdfcraft_fonts::embedding::face_key(&program.face)
                        && p.resource != program.resource
                })
                .filter_map(|p| families.get(&p.resource).cloned()),
        );
        if let Some(fallback) = definitions.families.get(&FontFamily::Proportional) {
            stack.extend(fallback.iter().cloned());
        }
        definitions.families.insert(FontFamily::Name(name.clone().into()), stack);
    }
    if !families.is_empty() {
        ctx.set_fonts(definitions);
    }
    families
}

fn editor_job(ui: &egui::Ui, ed: &LineEditor, text: &str, scale: f32, width: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    // An unchanged PDF row already has an authoritative line break. Do not rewrap it
    // because font rounding or a fallback face measures a fraction of a pixel wider.
    job.wrap.max_width = if text == ed.original && ed.look == ed.look0 && ed.extras == ed.extras0 { f32::INFINITY } else { width };
    if let Some((block, lines)) = &ed.source {
        let lefts: Vec<_> = block.rows.iter().filter_map(|r| r.first().and_then(|i| lines.get(*i)).map(|l| l.rect[0])).collect();
        let left_span = lefts.iter().copied().fold(f64::NEG_INFINITY, f64::max) - lefts.iter().copied().fold(f64::INFINITY, f64::min);
        let centered = block.rows.len() > 1
            && left_span > block.size * 0.5
            && block.rows.iter().all(|row| {
                let rect = row
                    .iter()
                    .filter_map(|i| lines.get(*i))
                    .map(|l| l.rect)
                    .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]);
                rect.is_some_and(|r| ((r[0] + r[2] - block.rect[0] - block.rect[2]) / 2.0).abs() < block.size)
            });
        if centered {
            job.halign = egui::Align::Center;
        }
        for (range, index) in block.edited_spans(text) {
            let Some(line) = lines.get(index) else { continue };
            let mut look = source_look(&line.base_font, line.size, line.color, line.bold, line.italic);
            look.size = line.size;
            if ed.look.family != ed.look0.family {
                look.family = ed.look.family;
            }
            if ed.look.bold != ed.look0.bold {
                look.bold = ed.look.bold;
            }
            if ed.look.italic != ed.look0.italic {
                look.italic = ed.look.italic;
            }
            if ed.look.color != ed.look0.color {
                look.color = ed.look.color;
            }
            if ed.look.size != ed.look0.size {
                look.size = ed.look.size;
            }
            let mut font = editor_font(&look, (look.size as f32 * scale).clamp(1.0, 300.0));
            let original_face = ed.look.family == ed.look0.family && ed.look.bold == ed.look0.bold && ed.look.italic == ed.look0.italic;
            let embedded = original_face && ed.fonts.contains_key(&line.font);
            if embedded && let Some(name) = ed.fonts.get(&line.font) {
                font.family = FontFamily::Name(name.clone().into());
            }
            let native_width = ui.fonts_mut(|f| f.layout_no_wrap(line.text.clone(), font.clone(), Color32::BLACK).size().x);
            let target_width = ((line.rect[2] - line.rect[0]) * look.size / line.size.max(0.01)) as f32 * scale;
            let spacing = if original_face { (target_width - native_width) / line.text.chars().count().saturating_sub(1).max(1) as f32 } else { 0.0 };
            job.append(
                text.get(range).unwrap_or_default(),
                0.0,
                egui::TextFormat {
                    font_id: font,
                    extra_letter_spacing: spacing + ed.extras.char_spacing as f32 * scale,
                    color: color32(look.color),
                    italics: look.italic && !embedded,
                    ..Default::default()
                },
            );
        }
    } else {
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: editor_font(&ed.look, ed.size),
                color: color32(ed.look.color),
                italics: ed.look.italic,
                ..Default::default()
            },
        );
    }
    job
}

/// Align the editable galley's baselines to the source rows instead of egui's interface
/// leading. Glyphs, meshes, caret and selection receive the same transform.
fn editor_galley(ui: &egui::Ui, ed: &LineEditor, text: &str, scale: f32, width: f32) -> Arc<egui::Galley> {
    let mut job = editor_job(ui, ed, text, scale, width);
    let centered = job.halign == egui::Align::Center;
    job.halign = egui::Align::LEFT;
    let mut galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let Some((block, lines)) = &ed.source else { return galley };
    let sources: Vec<_> = block
        .rows
        .iter()
        .filter_map(|row| {
            let first = row.first().and_then(|i| lines.get(*i))?;
            let rect = row
                .iter()
                .filter_map(|i| lines.get(*i))
                .map(|l| l.rect)
                .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])])?;
            Some((first.baseline()[1], rect))
        })
        .collect();
    let Some((first_baseline, first_rect)) = sources.first().copied() else { return galley };
    let leading = if ed.extras.line_spacing > 0.0 {
        ed.extras.line_spacing * ed.look.size
    } else {
        let fallback = if block.lines.len() == 1 && !text.contains('\n') {
            block.lines.first().and_then(|i| lines.get(*i)).map_or(block.size * 1.2, |line| line.wrap_leading())
        } else {
            block.size * 1.5
        };
        sources.windows(2).map(|r| (r[0].0 - r[1].0).abs()).rfind(|v| *v > 0.0).unwrap_or(fallback)
    };
    let g = Arc::make_mut(&mut galley);
    let mut bounds = Rect::NOTHING;
    let mut meshes = Rect::NOTHING;
    for (index, placed) in g.rows.iter_mut().enumerate() {
        let (last_baseline, last_rect) = sources.last().copied().unwrap_or((first_baseline, first_rect));
        let (baseline, source_rect) = sources
            .get(index)
            .copied()
            .unwrap_or((last_baseline - index.saturating_add(1).saturating_sub(sources.len()) as f64 * leading, last_rect));
        let changed_size = ed.look.size != ed.look0.size;
        let ratio = if changed_size { ed.look.size / ed.look0.size.max(0.01) } else { 1.0 };
        let ascent = (source_rect[3] - sources.get(index).map_or(last_baseline, |s| s.0)) as f32 * scale * ratio as f32;
        let descent = (sources.get(index).map_or(last_baseline, |s| s.0) - source_rect[1]) as f32 * scale * ratio as f32;
        let base_y = (block.rect[3] - baseline) as f32 * scale;
        let row = Arc::make_mut(&mut placed.row);
        placed.pos.y = base_y - ascent;
        placed.pos.x = if centered { (width - row.size.x) / 2.0 } else { (source_rect[0] - block.rect[0]) as f32 * scale };
        for glyph in &mut row.glyphs {
            let dy = ascent - glyph.pos.y;
            glyph.pos.y = ascent;
            glyph.font_ascent = ascent;
            glyph.font_height = ascent + descent;
            glyph.line_height = ascent + descent;
            if !glyph.uv_rect.is_nothing() {
                for vertex in row.visuals.mesh.vertices.iter_mut().skip(glyph.first_vertex as usize).take(4) {
                    vertex.pos.y += dy;
                }
            }
        }
        row.size.y = (ascent + descent).max(1.0);
        row.visuals.mesh_bounds = row.visuals.mesh.vertices.iter().fold(Rect::NOTHING, |r, v| r.union(Rect::from_min_max(v.pos, v.pos)));
        bounds = bounds.union(Rect::from_min_size(placed.pos, row.size));
        meshes = meshes.union(row.visuals.mesh_bounds.translate(placed.pos.to_vec2()));
    }
    g.rect = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(bounds.right().max(0.0), bounds.bottom().max(0.0)));
    g.mesh_bounds = meshes;
    galley
}

/// The inline editor; returns the edit once the text is applied.
pub(crate) fn overlay(ctx: &egui::Context, view: &mut DocView, info: &DocInfo) -> Option<Edit> {
    // Clicks outside the document (the Format text panel) keep the paragraph open.
    let outside = ctx.input(|i| i.pointer.latest_pos()).is_some_and(|p| !view.viewport_rect().contains(p));
    let page = view.line_editor.as_ref()?.page;
    let xf = view.page_xform(page)?;
    let hold = view.objects.hold_editor;
    // An OCR word is invisible text over a scan: the textless background still shows the scanned
    // word, so the editor covers it, as the applied edit will.
    let background_ready = view
        .line_editor
        .as_ref()
        .is_some_and(|ed| view.text_background.ready(ed.page, ed.block) && !ed.source.as_ref().is_some_and(|(b, _)| b.invisible));
    let draft_rect = view.text_background.draft_rect.map(|r| xf.user_rect(info, page, r.map(|v| v as f32)));
    let ed = view.line_editor.as_mut()?;
    if hold {
        ed.focus = true;
    }
    // Reproject the source box every frame. The page may have been zoomed, scrolled or rotated
    // while the format panel was open.
    // A font definition becomes active at the start of the next egui pass. Keep the source
    // raster visible until then instead of flashing an unrelated interface face.
    if !ctx.fonts(|f| ed.fonts.values().all(|name| f.definitions().families.contains_key(&FontFamily::Name(name.clone().into())))) {
        ctx.request_repaint();
        return None;
    }
    ed.rect = xf.user_rect(info, ed.page, ed.source_rect);
    ed.max_width = ed.rect.width();
    let scale = (ed.rect.width() / (ed.source_rect[2] - ed.source_rect[0]).abs().max(0.01)).max(0.01);
    ed.size = (ed.look.size as f32 * scale).clamp(1.0, 300.0);
    let font = editor_font(&ed.look, ed.size);
    let text_color = color32(ed.look.color);
    let mut done = None;
    egui::Area::new(egui::Id::new("edit-text-line")).order(egui::Order::Foreground).fixed_pos(Pos2::new(ed.rect.left(), ed.rect.top())).show(
        ctx,
        |ui| {
            // The editor opens at the source size. Once text changes, show the real draft
            // extent: single rows grow toward the page edge, paragraphs rewrap at their
            // source width, and the visible frame grows/shrinks vertically with their rows.
            // An OCR phrase (several stretched words on one scanned line) also grows rightwards.
            let single_run = ed.source.as_ref().is_some_and(|(block, _)| block.lines.len() == 1 || block.invisible);
            let width = if !ed.multiline && single_run && !ed.text.contains('\n') && ed.has_changes() {
                let job = editor_job(ui, ed, &ed.text, scale, f32::INFINITY);
                let natural = ui.fonts_mut(|f| f.layout_job(job)).size().x;
                natural.max(ed.size * 0.4).min((xf.rect.right() - ed.rect.left() - 36.0 * scale).max(ed.rect.width()))
            } else {
                ed.rect.width().max(1.0)
            };
            ui.set_width(width);
            let measured = editor_galley(ui, ed, &ed.text, scale, width);
            ed.overflow = ed.has_changes() && (measured.rect.width() > ed.rect.width() + 1.5 || measured.rect.height() > ed.rect.height() + 1.5);
            let visible = Rect::from_min_size(ed.rect.min, measured.rect.size().max(egui::vec2(1.0, 1.0)));
            // Unsupported source programs use an interface fallback whose glyph advances
            // can differ from the PDF's. Keep every visible row inside the growing frame.
            let live = draft_rect.map_or(visible, |bounds| bounds.union(visible));
            ui.painter().rect_stroke(live.expand(2.0), CornerRadius::ZERO, Stroke::new(1.5, ACCENT), egui::StrokeKind::Outside);
            // White, unless the text is too pale to read on white (white text from a coloured
            // banner, #913): then a dark box, with a caret that shows on it.
            let fill = if background_ready { Color32::TRANSPARENT } else { editor_fill(text_color) };
            // Until the background worker finishes, cover the entire source box, including
            // lines the draft no longer occupies. Shrinking the text must never expose a copy.
            if !background_ready {
                ui.painter().rect_filled(ed.rect.expand(1.0), CornerRadius::ZERO, fill);
            }
            egui::Frame::NONE.fill(fill).show(ui, |ui| {
                // A textless PDF preview keeps its real page background through the transparent editor.
                // The temporary opaque cover uses the shared caret-contrast rule.
                ui.visuals_mut().text_cursor.stroke.color =
                    if background_ready { if luminance(text_color) < 0.5 { Color32::BLACK } else { Color32::WHITE } } else { caret_color_on(fill) };
                let rows = 1;
                let snapshot = ed.clone();
                let mut layouter =
                    |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| editor_galley(ui, &snapshot, text.as_str(), scale, wrap_width);
                let output = egui::TextEdit::multiline(&mut ed.text)
                    .id(egui::Id::new("edit-text-line-input"))
                    .font(font.clone())
                    .margin(egui::Margin::ZERO)
                    .layouter(&mut layouter)
                    .text_color(text_color)
                    .frame(egui::Frame::NONE)
                    .desired_width(width)
                    .desired_rows(rows)
                    .show(ui);
                if let Some(cursor) = output.cursor_range {
                    let selected = cursor.as_sorted_char_range();
                    let byte = |index| ed.text.char_indices().nth(index).map_or(ed.text.len(), |(at, _)| at);
                    ed.selection = (!selected.is_empty()).then(|| byte(selected.start.into())..byte(selected.end.into()));
                }
                let r = output.response;
                if r.changed() {
                    // The next pass measures the updated draft with the Apply/Save rewrite.
                    ctx.request_repaint();
                }
                if ed.focus {
                    r.request_focus();
                    ed.focus = false;
                }
                let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                let apply = ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
                if esc {
                    done = Some(false);
                } else if apply || (r.lost_focus() && !outside && !hold) {
                    done = Some(true);
                }
            });
        },
    );
    match done {
        Some(apply) => {
            let ed = view.line_editor.take()?;
            let style = ed.style();
            (apply && (ed.text != ed.original || style != pdfcraft_engine::BlockStyle::default())).then_some(Edit::EditTextBlock {
                page: ed.page,
                block: ed.block,
                text: ed.text,
                style,
            })
        }
        None => None,
    }
}

/// Caret colour that stays visible on a field filled with `fill`: black on light fills, white on dark ones.
pub(crate) fn caret_color_on(fill: Color32) -> Color32 {
    let luma = 299 * u32::from(fill.r()) + 587 * u32::from(fill.g()) + 114 * u32::from(fill.b());
    if luma > 127_500 { Color32::BLACK } else { Color32::WHITE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_background_is_cached_without_editing_and_released_on_cancel() {
        let mut app = crate::PdfCraftApp::new();
        let bytes = app.session.create_from_text("Preview", "Preview text").unwrap();
        app.open_bytes("preview.pdf", None, bytes.as_ref().clone()).unwrap();
        let doc = app.session.get(app.views[0].id).unwrap();
        let block = doc.text_blocks(0).remove(0);
        let source = doc.bytes.clone();
        let generation = doc.edit_generation();
        let ed = LineEditor {
            selection: None,
            font_resource: None,
            page: 0,
            block: 0,
            text: block.text.clone(),
            original: block.text.clone(),
            rect: Rect::NOTHING,
            source_rect: block.rect.map(|v| v as f32),
            multiline: block.rows.len() > 1,
            source: None,
            fonts: BTreeMap::new(),
            overflow: false,
            max_width: 400.0,
            size: block.size as f32,
            focus: true,
            look: look_of(&block),
            look0: look_of(&block),
            extras: Extras::default(),
            extras0: Extras::default(),
        };
        let ctx = egui::Context::default();
        let mut background = TextBackground::default();
        for _ in 0..100 {
            background.prepare(&ctx, doc, Some(&ed), 1.0);
            if background.ready(0, 0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(background.ready(0, 0));
        assert!(!background.pending());
        let texture = background.texture.as_ref().unwrap().id();
        background.prepare(&ctx, doc, Some(&ed), 1.0);
        assert_eq!(background.texture.as_ref().unwrap().id(), texture);
        assert_eq!(doc.bytes, source);
        assert_eq!(doc.edit_generation(), generation);
        assert!(!doc.dirty && doc.can_undo().is_none());
        background.prepare(&ctx, doc, None, 1.0);
        assert!(background.texture.is_none() && background.renderer.is_none());
    }

    #[test]
    fn pale_text_is_edited_on_a_dark_box() {
        // #913: white text from a coloured banner was typed onto a white box and vanished.
        for pale in [Color32::WHITE, Color32::from_rgb(0xFF, 0xFF, 0x00), Color32::from_gray(0xE0), Color32::from_rgb(0xFF, 0xA5, 0x00)] {
            assert_eq!(editor_fill(pale), DARK_EDITOR_FILL, "{pale:?}");
            assert!(contrast(pale, editor_fill(pale)) >= 4.5, "{pale:?} is readable");
        }
        // Ordinary dark or saturated text keeps the white box.
        for dark in [Color32::BLACK, Color32::from_rgb(0x1F, 0x2A, 0x6B), Color32::from_rgb(0xC0, 0x00, 0x00), Color32::from_gray(0x60)] {
            assert_eq!(editor_fill(dark), Color32::WHITE, "{dark:?}");
        }
    }

    #[test]
    fn caret_contrasts_with_its_fill() {
        assert_eq!(caret_color_on(Color32::WHITE), Color32::BLACK);
        assert_eq!(caret_color_on(Color32::from_rgb(0xFF, 0xFF, 0xF4)), Color32::BLACK);
        assert_eq!(caret_color_on(Color32::BLACK), Color32::WHITE);
        assert_eq!(caret_color_on(DARK_EDITOR_FILL), Color32::WHITE);
    }

    #[test]
    fn bullet_list_toggles_and_preserves_content() {
        let mut text = "First line\nSecond line\n\nThird line".to_string();
        assert!(!is_bulleted(&text));
        assert!(toggle_bullet_list(&mut text));
        assert!(is_bulleted(&text));
        assert_eq!(text, "• First line\n• Second line\n\n• Third line");

        // Toggling again removes bullets
        assert!(toggle_bullet_list(&mut text));
        assert!(!is_bulleted(&text));
        assert_eq!(text, "First line\nSecond line\n\nThird line");
    }

    #[test]
    fn numbered_list_toggles_and_preserves_content() {
        let mut text = "Item A\nItem B\n\nItem C".to_string();
        assert!(!is_numbered(&text));
        assert!(toggle_numbered_list(&mut text));
        assert!(is_numbered(&text));
        assert_eq!(text, "1. Item A\n2. Item B\n\n3. Item C");

        // Toggling again removes numbering
        assert!(toggle_numbered_list(&mut text));
        assert!(!is_numbered(&text));
        assert_eq!(text, "Item A\nItem B\n\nItem C");
    }

    #[test]
    fn superscript_and_subscript_conversion() {
        let mut ed = LineEditor {
            page: 0,
            block: 0,
            text: "E = mc2".to_string(),
            original: "E = mc2".to_string(),
            rect: egui::Rect::ZERO,
            source_rect: [0.0; 4],
            max_width: 100.0,
            overflow: false,
            selection: Some(6..7), // selecting '2'
            font_resource: None,
            fonts: std::collections::BTreeMap::new(),
            source: None,
            multiline: false,
            size: 12.0,
            focus: true,
            look: pdfcraft_engine::AddedText::default(),
            look0: pdfcraft_engine::AddedText::default(),
            extras: Extras::default(),
            extras0: Extras::default(),
        };

        assert!(toggle_superscript(&mut ed));
        assert_eq!(ed.text, "E = mc²");
        // Toggling superscript again returns to normal '2'
        assert!(toggle_superscript(&mut ed));
        assert_eq!(ed.text, "E = mc2");

        // Test subscript on H2O
        ed.text = "H2O".to_string();
        ed.selection = Some(1..2); // selecting '2'
        assert!(toggle_subscript(&mut ed));
        assert_eq!(ed.text, "H₂O");
        assert!(toggle_subscript(&mut ed));
        assert_eq!(ed.text, "H2O");
    }

    #[test]
    fn value_based_line_spacing_in_style() {
        let ed = LineEditor {
            page: 0,
            block: 0,
            text: "Paragraph text".to_string(),
            original: "Paragraph text".to_string(),
            rect: egui::Rect::ZERO,
            source_rect: [0.0; 4],
            max_width: 100.0,
            overflow: false,
            selection: None,
            font_resource: None,
            fonts: std::collections::BTreeMap::new(),
            source: None,
            multiline: false,
            size: 12.0,
            focus: true,
            look: pdfcraft_engine::AddedText::default(),
            look0: pdfcraft_engine::AddedText::default(),
            extras: Extras { line_spacing: 1.33, underline: true, ..Default::default() },
            extras0: Extras::default(),
        };
        let style = ed.style();
        assert_eq!(style.line_spacing, Some(1.33));
        assert_eq!(style.underline, Some(true));
    }
}
