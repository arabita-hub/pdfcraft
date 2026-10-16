//! Cached page/image layers: a signature follows each pointer frame without editing the PDF.

use egui::{ColorImage, TextureHandle, TextureOptions};
use pdfcraft_engine::{Document, Edit, TextAnnotationPreview};
use pdfcraft_render::{RenderPool, RenderRequest};

use crate::QuickTool;
use crate::comments::{CommentView, Gesture, PageCx};

#[derive(Default)]
pub(crate) struct SignatureDrag {
    key: Option<(u64, usize, usize)>,
    aspect_ratio: Option<f32>,
    layers: Option<Layers>,
}

/// What follows the pointer: a signature's picture, or typed text.
enum Content {
    Image {
        image: TextureHandle,
        opacity: f32,
        /// The page /Rotate the PDF draws the image turned back by.
        turn: i64,
    },
    Text(TextAnnotationPreview),
}

struct Layers {
    content: Content,
    renderer: RenderPool,
    background: Option<TextureHandle>,
    tag: Option<u64>,
    received_tag: Option<u64>,
    /// Keep the final position visible until the normal page raster catches up.
    settling: bool,
}

impl SignatureDrag {
    pub fn prepare(&mut self, ctx: &egui::Context, doc: &Document, selected: Option<(usize, usize)>, scale: f32) {
        let key = selected.map(|(p, i)| (doc.edit_generation(), p, i));
        if self.key != key {
            self.key = key;
            self.aspect_ratio = None;
            self.layers = None;
            if let Some((_, page, index)) = key
                && doc.info.annotations.iter().any(|a| a.page == page && a.index == index && a.subtype == "Stamp" && !a.locked)
                && let Ok(Some(preview)) = doc.image_signature_preview(page, index)
            {
                let [width, height] = preview.image.size();
                let ratio = width as f32 / height as f32;
                // As displayed, the image is turned by the page's rotation less the turn it is drawn back by.
                let shown = doc.info.pages.get(page).map_or(0, |p| i64::from(p.rotation)) - preview.turn;
                self.aspect_ratio = Some(if shown.rem_euclid(180) == 0 { ratio } else { ratio.recip() });
                self.layers = Some(Layers {
                    content: Content::Image {
                        opacity: preview.opacity,
                        turn: preview.turn,
                        image: ctx.load_texture(
                            "signature-drag-image",
                            ColorImage::from_rgba_unmultiplied(preview.image.size(), preview.image.rgba()),
                            TextureOptions::LINEAR,
                        ),
                    },
                    renderer: preview.renderer(),
                    background: None,
                    tag: None,
                    received_tag: None,
                    settling: false,
                });
            } else if let Some((_, page, index)) = key
                && let Ok(Some(preview)) = doc.text_annotation_preview(page, index)
            {
                self.layers = Some(Layers {
                    renderer: preview.renderer(),
                    content: Content::Text(preview),
                    background: None,
                    tag: None,
                    received_tag: None,
                    settling: false,
                });
            }
        }
        let (Some((_, page, _)), Some(layers)) = (self.key, self.layers.as_mut()) else { return };
        // The image stays sharp at every zoom. The background matches the page view up to one
        // whole-page texture; past that the view tiles and this preview stays at that cap.
        let Some(p) = doc.info.pages.get(page) else { return };
        let scale = scale.min(crate::canvas::BASE_SIDE / p.width.max(p.height).max(1.0));
        let tag = (scale * 1000.0) as u64;
        if layers.tag != Some(tag) {
            layers.renderer.set_queue(vec![RenderRequest { page, scale, tag, preview: true, ..Default::default() }]);
            layers.tag = Some(tag);
        }
        if let Some(out) = layers.renderer.try_recv()
            && out.request.tag == tag
        {
            if out.error.is_none() {
                layers.received_tag = Some(tag);
                layers.background = Some(ctx.load_texture(
                    "signature-drag-background",
                    ColorImage::from_rgba_premultiplied([out.width as usize, out.height as usize], &out.rgba),
                    TextureOptions::LINEAR,
                ));
            } else {
                // Leave the ordinary page raster usable if an unsupported PDF can't preview.
                self.layers = None;
                return;
            }
        }
        if layers.received_tag != Some(tag) {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    pub fn contains(&self, page: usize, index: usize) -> bool {
        self.key.is_some_and(|(_, p, i)| (p, i) == (page, index)) && self.layers.is_some()
    }

    /// Like [`Self::contains`], for a signature picture (which has no selection outline of its own).
    pub fn contains_image(&self, page: usize, index: usize) -> bool {
        self.contains(page, index) && matches!(self.layers, Some(Layers { content: Content::Image { .. }, .. }))
    }

    /// Use the embedded image's proportions, even after an edge handle stretched its rectangle: its
    /// width over its height as the page is displayed (before the view's own rotation).
    pub fn aspect_ratio(&self, page: usize, index: usize) -> Option<f32> {
        self.key.filter(|(_, p, i)| (*p, *i) == (page, index)).and(self.aspect_ratio)
    }

    /// Movement/resizing only change this signature's geometry, so its background is reusable.
    pub fn committed(&mut self, edit: &Edit, generation: u64) {
        if let Edit::MoveAnnotation { page, index, .. } | Edit::ResizeAnnotation { page, index, .. } = edit
            && self.contains(*page, *index)
        {
            self.key = Some((generation, *page, *index));
            if let Some(layers) = &mut self.layers {
                layers.settling = true;
            }
        } else {
            *self = Self::default();
        }
    }

    pub fn page_received(&mut self, page: usize) {
        if self.key.is_some_and(|(_, p, _)| p == page)
            && let Some(layers) = &mut self.layers
        {
            layers.settling = false;
        }
    }

    pub fn paint(&self, painter: &egui::Painter, cx: &PageCx<'_>, cv: &CommentView, pending: Option<&Edit>) {
        let (Some((_, page, index)), Some(layers)) = (self.key, &self.layers) else { return };
        if page != cx.page || !matches!(cx.tool, QuickTool::Select | QuickTool::Fill(_)) || cx.hidden {
            return;
        }
        let dragging = matches!(cv.gesture, Some(Gesture::Move { page: p, index: i, .. } | Gesture::Resize { page: p, index: i, .. }) if (p, i) == (page, index));
        let released = matches!(pending, Some(Edit::MoveAnnotation { page: p, index: i, .. } | Edit::ResizeAnnotation { page: p, index: i, .. }) if (*p, *i) == (page, index));
        if !(dragging || released || layers.settling) {
            return;
        }
        let (Some(background), Some(a), Some(p)) = (&layers.background, cx.get(index), cx.info.pages.get(page)) else { return };
        let (pointer, shift) = painter.ctx().input(|i| (i.pointer.hover_pos(), i.modifiers.shift));
        let screen = cx.adjusted_rect(a, cv.gesture.as_ref(), pointer, shift, pending);
        let painter = painter.with_clip_rect(painter.clip_rect().intersect(cx.xf.rect));
        match &layers.content {
            Content::Image { image, opacity, turn } => {
                cx.xf.paint_image(&painter, background.id(), 0.0, 0.0, 1.0, 1.0);
                cx.xf.paint_user_image(
                    &painter,
                    image.id(),
                    p,
                    cx.rect_to_user(screen),
                    *turn,
                    egui::Color32::from_white_alpha((opacity * 255.0).round() as u8),
                );
            }
            Content::Text(text) => {
                // Only the upright view: a turned view keeps the ordinary page until the drop.
                if !cx.xf.rot.is_multiple_of(360) {
                    return;
                }
                cx.xf.paint_image(&painter, background.id(), 0.0, 0.0, 1.0, 1.0);
                paint_text(&painter, cx, text, screen);
            }
        }
    }
}

/// The text laid out in `rect` (screen space) as its appearance draws it: left aligned, wrapped
/// to the box, in the box's size, slant and colour.
fn paint_text(painter: &egui::Painter, cx: &PageCx<'_>, text: &TextAnnotationPreview, rect: egui::Rect) {
    use pdfcraft_engine::annot_text::wrap;
    let zoom = cx.xf.rect.width() / cx.xf.pw.max(1.0);
    if !(zoom.is_finite() && zoom > 0.0) {
        return;
    }
    let pad = 2.0;
    let k = if text.bold { 1.08 } else { 1.0 };
    let size = text.size;
    let width = f64::from(rect.width() / zoom) - 2.0 * pad;
    let [r, g, b] = text.color.map(|x| (x.clamp(0.0, 1.0) * 255.0).round() as u8);
    let color = egui::Color32::from_rgb(r, g, b);
    let font = egui::FontId::proportional((size as f32 * zoom).max(1.0));
    let painter = painter.with_clip_rect(painter.clip_rect().intersect(rect));
    for (i, line) in wrap(&text.text, size * k, width.max(1.0)).iter().enumerate() {
        let baseline = rect.top() + (pad + size * 0.9 + i as f64 * size * 1.2) as f32 * zoom;
        let at = egui::pos2(rect.left() + pad as f32 * zoom, baseline + size as f32 * 0.2 * zoom);
        let mut job = egui::text::LayoutJob::default();
        job.append(line, 0.0, egui::TextFormat { font_id: font.clone(), color, italics: text.italic, ..Default::default() });
        let galley = painter.layout_job(job);
        let origin = egui::pos2(at.x, at.y - galley.size().y);
        painter.galley(origin, galley.clone(), color);
        if text.bold {
            // egui's built-in font has no bold weight: a second pass a hair to the right.
            painter.galley(origin + egui::vec2(0.6 * zoom.max(1.0), 0.0), galley, color);
        }
    }
}
