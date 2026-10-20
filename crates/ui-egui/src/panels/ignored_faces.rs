//! Model-independent review and restoration of persistent ignored face crops.
use std::collections::HashSet;

use egui::{Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde_json::{Value, json};

use crate::{LightcraftApp, widgets::register};

#[derive(Clone, Debug, Default)]
pub struct View {
    faces: Option<std::sync::Arc<Vec<Value>>>,
    selected: HashSet<(u64, usize)>,
    revision: Option<u64>,
    anchor: Option<(usize, bool)>,
    error: Option<String>,
}

impl View {
    /// Refresh after catalog/history changes and discard potentially stale selection keys.
    pub(crate) fn invalidate(&mut self) {
        *self = Self::default();
    }
}

/// Read a retained catalog region's stable review key.
fn key(face: &Value) -> Option<(u64, usize)> {
    Some((face["photo"].as_u64()?, usize::try_from(face["index"].as_u64()?).ok()?))
}

/// Show retained crops without loading a recognition model or constructing unnamed groups.
pub fn show(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    let Some(mut view) = app.ui.ignored_faces.take() else { return };
    if view.revision.is_some_and(|revision| revision != app.session.catalog.revision) {
        view.invalidate();
    }
    view.revision = Some(app.session.catalog.revision);
    let mut back = false;
    egui::Frame::NONE.inner_margin(16).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let button = ui.button("‹ People");
            register(ui.ctx(), "ignoredFaces:back", button.rect);
            back = button.clicked();
            ui.heading("Ignored faces");
            let button = ui.button("Refresh ignored faces");
            register(ui.ctx(), "ignoredFaces:refresh", button.rect);
            if button.clicked() {
                view.invalidate();
            }
        });
        if view.faces.is_none() && view.error.is_none() {
            view.revision = Some(app.session.catalog.revision);
            match app.session.execute("faces.ignored", &json!({})) {
                Ok(response) => view.faces = Some(std::sync::Arc::new(response["faces"].as_array().cloned().unwrap_or_default())),
                Err(error) => view.error = Some(error.to_string()),
            }
        }
        if let Some(error) = &view.error {
            ui.label(error);
        }
        let faces = view.faces.clone().unwrap_or_default();
        let mut restore = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Ignored faces: {} · Selected: {}", faces.len(), view.selected.len()));
            let button = ui.button("Select all");
            register(ui.ctx(), "ignoredFaces:selectAll", button.rect);
            if button.clicked() {
                view.selected.extend(faces.iter().filter_map(key));
            }
            let button = ui.button("Clear selection");
            register(ui.ctx(), "ignoredFaces:clear", button.rect);
            if button.clicked() {
                view.selected.clear();
                view.anchor = None;
            }
            let button = ui.add_enabled(!view.selected.is_empty(), egui::Button::new("Restore selected faces"));
            register(ui.ctx(), "ignoredFaces:restore", button.rect);
            restore = button.clicked();
        });
        ui.label("Click faces to select or deselect. Shift-click extends that action to a range. Restoration keeps the original name and box.");
        if restore {
            let selected: Vec<_> = faces.iter().filter(|f| key(f).is_some_and(|k| view.selected.contains(&k))).cloned().collect();
            match app.run("faces.ignore", json!({"faces": selected, "ignored": false})) {
                Ok(_) => view.invalidate(),
                Err(error) => view.error = Some(error),
            }
            return;
        }
        if faces.is_empty() {
            ui.label("No ignored faces.");
        }
        let edge = super::people::card_edge(app.ui.thumb_size);
        let cols = ((ui.available_width() + 12.0) / (edge + 12.0)).floor().max(1.0) as usize;
        let mut shown = 0;
        egui::ScrollArea::vertical().show_rows(ui, edge + 32.0, faces.len().div_ceil(cols), |ui, rows| {
            for row in rows {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let index = row.saturating_mul(cols).saturating_add(col);
                        let Some(face) = faces.get(index) else { break };
                        let Some(face_key) = key(face) else { continue };
                        let (area, _) = ui.allocate_exact_size(vec2(edge, edge + 24.0), Sense::hover());
                        let image = Rect::from_min_size(area.min, vec2(edge, edge));
                        let response = ui.interact(image, ui.id().with(("ignored-face", face_key)), Sense::click());
                        register(ui.ctx(), format!("ignoredFaces:face:{index}"), image);
                        if let Ok(rect) = serde_json::from_value(face["view"].clone())
                            && let Some(job) = app.session.face_job(
                                lightcraft_catalog::PhotoId(face_key.0),
                                rect,
                                (edge * ui.ctx().pixels_per_point()).ceil() as usize,
                            )
                            && let Some(texture) = app.renderer.variant(job)
                        {
                            ui.painter().image(texture.tex.id(), image, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        }
                        shown += 1;
                        if response.clicked() {
                            let (start, selecting) =
                                view.anchor.filter(|_| ui.input(|i| i.modifiers.shift)).unwrap_or((index, !view.selected.contains(&face_key)));
                            for key in faces.get(start.min(index)..=start.max(index)).unwrap_or_default().iter().filter_map(key) {
                                if selecting {
                                    view.selected.insert(key);
                                } else {
                                    view.selected.remove(&key);
                                }
                            }
                            if !ui.input(|i| i.modifiers.shift) || view.anchor.is_none() {
                                view.anchor = Some((index, selecting));
                            }
                        }
                        let selected = view.selected.contains(&face_key);
                        if selected {
                            ui.painter().rect_stroke(image, 3.0, Stroke::new(2.0, Color32::from_rgb(110, 160, 255)), StrokeKind::Inside);
                        }
                        crate::widgets::announce_selected(&response, "Ignored face", selected);
                        ui.painter().text(
                            pos2(area.left(), image.bottom() + 12.0),
                            egui::Align2::LEFT_CENTER,
                            if selected { "✓ Selected" } else { "Not selected" },
                            egui::FontId::proportional(12.0),
                            Color32::WHITE,
                        );
                    }
                });
            }
        });
        app.renderer.want_variants(shown);
    });
    if !back {
        app.ui.ignored_faces = Some(view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Restore a retained assignment through the actual panel without any recognition model.
    #[test]
    fn ignored_review_restores_without_a_model_and_back_returns_to_people() {
        let mut app = LightcraftApp::new(lightcraft_engine::Session::with_demo(), crate::Services::default());
        let id = app.session.catalog.photos().next().unwrap().id;
        let mut meta = app.session.catalog.photo(id).unwrap().meta.clone();
        let rect = lightcraft_geom::Rect { x0: 0.2, y0: 0.2, x1: 0.6, y1: 0.6 };
        meta.regions =
            vec![lightcraft_meta::Region { rect, kind: lightcraft_meta::RegionKind::IgnoredFace, name: Some("Person".into()), description: None }];
        app.session.commit("setup", lightcraft_catalog::Op::SetMeta { id, meta: Box::new(meta) }).unwrap();
        let mut h = crate::headless::Headless::new(app, [1400.0, 900.0], 1.0);
        let timeout = std::time::Duration::from_secs(15);
        h.step();
        h.app.run("view.ignoredFaces", json!({})).unwrap();
        h.settle(timeout);
        for widget in ["ignoredFaces:selectAll", "ignoredFaces:restore"] {
            let response = h.request("ui.clickWidget", json!({"id": widget}), timeout);
            assert_eq!(response["ok"], true, "{response}");
            h.step();
        }
        let region = &h.app.session.catalog.photo(id).unwrap().meta.regions[0];
        assert_eq!(region.kind, lightcraft_meta::RegionKind::Face);
        assert_eq!(region.name.as_deref(), Some("Person"));
        assert_eq!(region.rect, rect);
        h.app.run("edit.undo", json!({})).unwrap();
        assert_eq!(h.app.session.catalog.photo(id).unwrap().meta.regions[0].kind, lightcraft_meta::RegionKind::IgnoredFace);
        h.step();
        assert_eq!(h.app.ui.ignored_faces.as_ref().unwrap().faces.as_ref().unwrap().len(), 1);
        h.app.run("edit.redo", json!({})).unwrap();
        h.step();
        assert!(h.app.ui.ignored_faces.as_ref().unwrap().faces.as_ref().unwrap().is_empty());
        h.app.run("edit.undo", json!({})).unwrap();
        h.step();
        // Direct session calls bypass the UI command hook, including library membership changes.
        h.app.session.selection = lightcraft_engine::Selection::single(id);
        let selected = h.request("ui.clickWidget", json!({"id": "ignoredFaces:selectAll"}), timeout);
        assert_eq!(selected["ok"], true);
        h.step();
        assert_eq!(h.app.ui.ignored_faces.as_ref().unwrap().selected.len(), 1);
        h.app.session.execute("photo.delete", &json!({"ids": [id.0]})).unwrap();
        h.step();
        assert!(h.app.ui.ignored_faces.as_ref().unwrap().faces.as_ref().unwrap().is_empty());
        assert!(h.app.ui.ignored_faces.as_ref().unwrap().selected.is_empty());
        h.app.session.execute("photo.restore", &json!({"ids": [id.0]})).unwrap();
        h.step();
        assert_eq!(h.app.ui.ignored_faces.as_ref().unwrap().faces.as_ref().unwrap().len(), 1);
        h.app.session.execute("faces.ignore", &json!({"faces": [{"photo": id.0, "index": 0}], "ignored": false})).unwrap();
        h.step();
        assert!(h.app.ui.ignored_faces.as_ref().unwrap().faces.as_ref().unwrap().is_empty());
        h.app.session.execute("edit.undo", &json!({})).unwrap();
        h.step();
        if let Some(path) = std::env::var_os("LIGHTCRAFT_IGNORED_SCREENSHOT") {
            let inspected = h.request("ui.inspect", json!({}), timeout);
            println!("Ignored-face review perf: {}", inspected["result"]["perf"]);
            let shot = h.snapshot(timeout);
            let image = lightcraft_raster::Rgba8::from_fn(shot.size[0], shot.size[1], |x, y| shot.pixels[y * shot.size[0] + x].to_array());
            let png = lightcraft_engine::export::encode_image(
                &image,
                &lightcraft_engine::export::ExportOptions { format: lightcraft_engine::export::ExportFormat::Png, ..Default::default() },
            )
            .unwrap();
            std::fs::write(path, png).unwrap();
        }
        h.app.run("view.back", json!({})).unwrap();
        assert!(h.app.ui.ignored_faces.is_none());
    }
}
