//! The import review dialog (File → Import Photos… / Import from Folder… / Import from Device):
//! the source it scanned (a scanned folder is *not* added to Local), the files found under it as
//! a grid of thumbnails with checkboxes (duplicates marked and unchecked), the destination (add in
//! place / copy or move into the library's `Originals/` or a chosen folder, filed by day, by month,
//! into one folder or by a custom folder template, optionally renamed), an album
//! (existing or new), a preset and keywords to apply. Importing runs on a worker thread (files are
//! probed, copied or moved there) and its batches join the catalog between frames, with a progress
//! window and Cancel; the whole import is one undo step.

use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use lightcraft_engine::Session;
use lightcraft_engine::import::{ImportCandidate, ScanInput, ScanOutput, ScanProgress, scan_with};
use lightcraft_engine::import::ImportCandidate;
use lightcraft_engine::media::ProbeInfo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::LightcraftApp;
use crate::render::Slot;
use crate::theme::Tokens;
use crate::widgets::register;

/// Files per batch (each batch joins the catalog as it is ready, so the progress window updates).
pub(crate) const BATCH: usize = 8;

pub type ImportScanResult = (Vec<ImportCandidate>, HashMap<String, ProbeInfo>);

#[derive(Debug)]
pub struct ImportScanTask {
    receiver: Receiver<ImportScanResult>,
    progress: lightcraft_engine::import::ImportScanProgressState,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImportDialog {
    /// The files / folders that were scanned (shown as the source).
    pub sources: Vec<String>,
    pub candidates: Vec<ImportCandidate>,
    pub checked: Vec<bool>,
    /// Copy into the library (else add in place).
    pub copy: bool,
    /// With `copy`: move instead — the originals are removed from the source once each copy is
    /// verified and catalogued (`library.import` mode `move`).
    pub move_files: bool,
    /// Existing album to add to.
    pub album: Option<u64>,
    /// …or a new album with this name.
    pub new_album: String,
    /// Preset id ("" = none).
    pub preset: String,
    /// Comma-separated keywords.
    pub keywords: String,
    /// Copy: destination folder ("" = the library's Originals/).
    pub destination: String,
    /// Copy: `date` (YYYY/YYYY-MM-DD), `month`, `flat` or `custom` ([`ImportDialog::folder_template`]).
    pub organize: String,
    /// Copy, `custom`: the folder template, e.g. `{date:%Y}/{date:%Y%m%d}`.
    pub folder_template: String,
    /// Copy: file-name template for the copies ("" = keep the names).
    pub rename: String,
    /// Metadata preset name ("" = none).
    pub metadata_preset: String,
    /// Copy: raws are copied as DNG.
    pub dng: bool,
    /// Apply Auto to selected photos without XMP.
    pub auto_without_xmp: bool,
}

impl ImportDialog {
    pub fn new(candidates: Vec<ImportCandidate>) -> Self {
        let checked = candidates.iter().map(|c| c.duplicate.is_none() && c.error.is_none()).collect();
        ImportDialog { candidates, checked, ..Default::default() }
    }
    pub fn importable(&self, i: usize) -> bool {
        self.candidates.get(i).is_some_and(|c| c.duplicate.is_none() && c.error.is_none())
    }
    pub fn selected_paths(&self) -> Vec<String> {
        self.candidates
            .iter()
            .zip(&self.checked)
            .filter(|(c, on)| **on && c.duplicate.is_none() && c.error.is_none())
            .map(|(c, _)| c.path.clone())
            .collect()
    }
    /// The `organize` param of `library.import` (`None` = the default, by day); an unusable
    /// custom folder template is an error.
    pub fn organize_param(&self) -> Result<Option<String>, String> {
        match self.organize.as_str() {
            "" => Ok(None),
            "custom" => {
                let t = self.folder_template.trim();
                if let Some(e) = lightcraft_engine::rename::folder_template_error(t) {
                    return Err(e);
                }
                // a plain folder name ("Imports") is a one-level template too
                Ok(Some(if t.contains(['{', '/', '\\']) { t.to_string() } else { format!("{t}/") }))
            }
            o => Ok(Some(o.to_string())),
        }
    }
    fn keywords(&self) -> Vec<String> {
        self.keywords.split(',').map(str::trim).filter(|k| !k.is_empty()).map(str::to_string).collect()
    }
}

/// A running import (see the module docs). The file-system work — expanding folders, probing,
/// reading sidecars, copying or placing files — runs on a worker thread
/// ([`lightcraft_engine::import::ImportJob`]); each batch it readies is added to the catalog on the
/// UI thread between frames, so a slow drive never stalls the window.
#[derive(Debug, Default)]
pub struct ImportTask {
    /// Files and folders to import (handed to the worker when the task starts).
    queue: Vec<String>,
    pub total: usize,
    pub done: usize,
    params: Value,
    pub imported: usize,
    pub duplicates: usize,
    pub failed: usize,
    /// Move: originals moved, and sources left in place (reported by the engine with a reason).
    pub moved: usize,
    pub kept: usize,
    undo0: usize,
    first: Option<u64>,
    pub kept: usize,
    undo0: usize,
    first: Option<u64>,
    preserve_selection: bool,
    selection_before: lightcraft_engine::Selection,
    /// Reading a folder for the Local view: the photos stay out of the library, and nothing is
    /// selected or announced as added.
    browse: bool,
    /// Cancel was pressed: no further files are started; batches already readied are added.
    pub cancelled: bool,
    /// Auto Import (the watched folder): the selection stays as it is, a short toast when done.
    auto: bool,
    /// Auto Import: the selection to keep.
    keep_selection: Option<lightcraft_engine::Selection>,
    run: Option<ImportRun>,
}

impl ImportTask {
    /// An import of `paths` (files or folders) with `library.import` params (without `paths`).
    pub fn new(paths: Vec<String>, params: Value, undo0: usize, browse: bool) -> Self {
        ImportTask { total: paths.len(), queue: paths, params, undo0, browse, ..Default::default() }
    }

    /// An Auto Import (see [`ImportTask::auto`]).
    pub fn auto(mut self) -> Self {
        self.auto = true;
        self
    }

    /// `{done, total, imported, cancelled}` for `ui.inspect`.
    pub fn status(&self) -> Value {
        json!({"done": self.done, "total": self.total, "imported": self.imported, "cancelled": self.cancelled})
    }
}
}

/// Start importing the dialog's checked files (the dialog's OK / `ui.dialog.confirm`).
pub fn start(app: &mut LightcraftApp, d: &ImportDialog) -> Result<Value, String> {
    let queue = d.selected_paths();
    if queue.is_empty() {
        return Err("no photos selected".into());
    }
    let undo0 = app.session.undo.len();
    let mut album = d.album;
    if album.is_none() && !d.new_album.trim().is_empty() {
        let r = app.session.execute("album.create", &json!({"name": d.new_album.trim()})).map_err(|e| e.to_string())?;
        album = r["id"].as_u64();
    }
    let mode = match (d.copy, d.move_files) {
        (false, _) => "add",
        (true, false) => "copy",
        (true, true) => "move",
    };
    let mut params = json!({"mode": mode, "keywords": d.keywords()});
    if let Some(a) = album {
        params["album"] = json!(a);
    }
    if !d.preset.is_empty() {
        params["preset"] = json!(d.preset);
    }
    if !d.metadata_preset.is_empty() {
        params["metadataPreset"] = json!(d.metadata_preset);
    }
    if d.auto_without_xmp {
        params["autoWithoutXmp"] = json!(true);
    }
    if d.copy {
        if !d.destination.trim().is_empty() {
            params["destination"] = json!(d.destination.trim());
        }
        if let Some(o) = d.organize_param()? {
            params["organize"] = json!(o);
        }
        if !d.rename.trim().is_empty() {
            params["rename"] = json!(d.rename.trim());
            params["renameStart"] = json!(1);
        }
        if d.dng && !d.move_files {
            params["dng"] = json!(true);
        }
    }
    let total = queue.len();
    let preserve_selection = d.auto_without_xmp;
    let selection_before = app.session.selection.clone();
    app.import = Some(ImportTask { queue, total, params, undo0, preserve_selection, selection_before, ..Default::default() });
    app.renderer.forget_imports();
    Ok(json!({"importing": total}))
    app.renderer.forget_imports();
    Ok(json!({"importing": total}))
}

/// Start importing `paths` (files or folders) in the background, e.g. dropped on the window.
pub fn start_paths(app: &mut LightcraftApp, paths: Vec<String>) -> Result<Value, String> {
    if app.import.is_some() || app.scan.as_ref().is_some_and(|t| !t.browse) {
        return Err("an import is running".into());
    }
    let r = app.session.execute("library.import", &p);
    let task = app.import.as_mut().expect("import task");
    if task.preserve_selection {
        app.session.selection = task.selection_before.clone();
    }
    task.done += n;
    let undo0 = app.session.undo.len();
    let total = paths.len();
    app.import = Some(ImportTask::new(paths, json!({"mode": "add"}), undo0, false));
    Ok(json!({"importing": total}))
}

/// Advance the import in progress (called every frame): start its worker, then add the batches it
/// has readied to the catalog.
pub fn tick(app: &mut LightcraftApp, ctx: &egui::Context) {
    let Some(mut task) = app.import.take() else { return };
    if task.run.is_none() {
        let mut p = task.params.clone();
        p["paths"] = json!(task.queue);
        let started = lightcraft_engine::cmd::library::import_params(&app.session, &p)
            .and_then(|req| Ok((lightcraft_engine::import::ImportJob::new(&mut app.session, req.opts)?, req.album, req.album_name)))
            .map_err(|e| e.to_string())
            .and_then(|(job, album, name)| ImportRun::start(job, std::mem::take(&mut task.queue), album, name, ctx));
        if task.auto {
            task.keep_selection = Some(app.session.selection.clone());
        }
        match started {
            Ok(run) => task.run = Some(run),
            Err(e) => {
                log::warn!("import: {e}");
                app.toast(ctx, format!("Import failed: {e}"));
                return;
            }
        }
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(100));
    let Some(run) = task.run.as_mut() else { return };
    let batches = run.next_batches();
    let finished = batches.is_none();
    for prepared in batches.into_iter().flatten() {
        commit_batch(app, &mut task, prepared);
        if let Some(sel) = &task.keep_selection {
            app.session.selection = sel.clone();
        }
    }
    if let Some(run) = &task.run {
        task.total = run.total.load(Ordering::Relaxed).max(task.total);
        task.done = run.done.load(Ordering::Relaxed);
    }
    if !finished {
        app.import = Some(task);
        return;
    }
    finish(app, ctx, task);
}
    match r {
        Ok(v) => {
            // a new album (named in the params) is created with the first photos; later batches join it
            if let (Some(a), Some(run)) = (v["album"].as_u64(), task.run.as_mut()) {
                run.album = Some(a);
            }
            let len = |k: &str| v[k].as_array().map_or(0, Vec::len);
            task.imported += len("imported");
            task.duplicates += len("duplicates");
            task.failed += len("failed");
            task.moved += len("moved");
            task.kept += len("kept");
            for k in v["kept"].as_array().into_iter().flatten() {
                log::warn!("import: kept {}: {}", k["path"].as_str().unwrap_or(""), k["reason"].as_str().unwrap_or(""));
            }
            if task.first.is_none() {
                task.first = v["imported"].as_array().and_then(|a| a.first()).and_then(Value::as_u64);
            }
        }
        Err(e) => {
            log::warn!("import: {e}");
            task.failed += n;
        }
    }
}

/// The import is done (or cancelled): one undo step, select the first photo, say what happened.
fn finish(app: &mut LightcraftApp, ctx: &egui::Context, task: ImportTask) {
    let steps = app.session.undo.len().saturating_sub(task.undo0);
    let label = crate::i18n::tr_format!("Add {} Photo{}", task.imported, if task.imported == 1 { "" } else { "s" });
    app.session.merge_undo(steps, &label);
    if task.auto {
        if task.imported > 0 {
            app.toast(ctx, format!("Auto Import: added {} photo{}", task.imported, if task.imported == 1 { "" } else { "s" }));
        }
        return;
    }
    if task.browse {
        if task.failed > 0 {
            app.toast(ctx, crate::i18n::tr_format!("{} photo{} not readable", task.failed, if task.failed == 1 { "" } else { "s" }));
        }
        return;
    }
    let task = app.import.take().expect("import task");
    let steps = app.session.undo.len().saturating_sub(task.undo0);
    let label = format!("Add {} Photo{}", task.imported, if task.imported == 1 { "" } else { "s" });
    app.session.merge_undo(steps, &label);
    if !task.preserve_selection
        && let Some(f) = task.first
    {
        let _ = app.run("library.select", json!({"ids": [f]}));
    }
    if let Some(f) = task.first {
        let _ = app.run("library.select", json!({"ids": [f]}));
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    // ...
    return;
        let _ = app.run("library.select", json!({"ids": [f]}));
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut msg = if task.cancelled {
        crate::i18n::tr_format!("Import cancelled · {} photo{} added", task.imported, plural(task.imported))
    } else {
        crate::i18n::tr_format!("Imported {} photo{}", task.imported, plural(task.imported))
    };
    if task.params["mode"] == "move" {
        msg.push_str(&crate::i18n::tr_format!(" · {} moved", task.moved));
        if task.kept > 0 {
            msg.push_str(&format!(" · {} original{} left at the source", task.kept, plural(task.kept)));
        }
    }
    if task.duplicates > 0 {
        msg.push_str(&crate::i18n::tr_format!(" · {} duplicate{} skipped", task.duplicates, if task.duplicates == 1 { "" } else { "s" }));
    }
    if task.failed > 0 {
        msg.push_str(&crate::i18n::tr_format!(" · {} not readable", task.failed));
    }
    app.toast(ctx, msg);
}

/// The progress window while an import runs, with Cancel (files already copied or added stay;
/// nothing new is started).
pub fn progress(app: &mut LightcraftApp, ctx: &egui::Context) {
    if let Some(task) = &app.import_scan {
        let t = Tokens::get(ctx);
        let status = task.progress.lock().map(|p| p.clone()).unwrap_or_default();
        egui::Window::new("Scanning folder")
            .title_bar(false)
            .resizable(false)
            .anchor(Align2::CENTER_BOTTOM, [0.0, -80.0])
            .fixed_size([620.0, 146.0])
            .show(ctx, |ui| {
                let label = if status.total == 0 {
                    format!("{} · {} photos found", status.phase, status.done)
                } else {
                    format!("{} · {} of {}", status.phase, status.done, status.total)
                };
                ui.label(egui::RichText::new(label).color(t.text));
                if status.total > 0 {
                    ui.add(egui::ProgressBar::new(status.done as f32 / status.total as f32).desired_width(f32::INFINITY));
                } else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Searching folders; total file count is not known yet");
                    });
                }
                for path in status.recent_paths.iter().rev() {
                    ui.add(egui::Label::new(egui::RichText::new(path).small().color(t.text_dim)).truncate()).on_hover_text(path);
                }
            });
        return;
    }
    let Some(task) = &app.import else { return };
    let t = Tokens::get(ctx);
    let frac = task.done as f32 / task.total.max(1) as f32;
    let text = if task.cancelled {
        crate::i18n::tr("Stopping…").to_string()
    } else if task.browse {
        crate::i18n::tr_format!("Reading photos… {} of {}", task.done, task.total)
    } else {
        crate::i18n::tr_format!("Adding photos… {} of {}", task.done, task.total)
    };
    let mut cancel = false;
    egui::Window::new(crate::i18n::tr("Importing"))
        .title_bar(false)
        .resizable(false)
        .anchor(Align2::CENTER_BOTTOM, [0.0, -80.0])
        .fixed_size([340.0, 80.0])
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(text).color(t.text));
            ui.add(egui::ProgressBar::new(frac).desired_width(320.0));
            let r = ui.add_enabled(!task.cancelled, egui::Button::new(crate::i18n::tr("Cancel")));
            register(ui.ctx(), "button:importCancel", r.rect);
            cancel = r.clicked();
        });
    if cancel && let Some(task) = app.import.as_mut() {
        task.cancelled = true;
        if let Some(run) = &task.run {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// The dialog body: options, then the candidate grid.
pub fn body(app: &mut LightcraftApp, ui: &mut egui::Ui, d: &mut ImportDialog) {
    let t = Tokens::get(ui.ctx());
    let n = d.candidates.len();
    let (dups, sel) = d.candidates.iter().zip(&d.checked).fold((0, 0), |(dups, sel), (c, checked)| {
        (dups + usize::from(c.duplicate.is_some()), sel + usize::from(*checked && c.duplicate.is_none() && c.error.is_none()))
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(crate::i18n::tr_format!("{n} found · {sel} selected", n = n, sel = sel)).color(t.text));
        if dups > 0 {
            ui.label(egui::RichText::new(crate::i18n::tr_format!("· {dups} already in the library", dups = dups)).color(t.text_dim));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::widgets::text_button(ui, "importNone", crate::i18n::tr("Uncheck All"), false).clicked() {
                d.checked.iter_mut().for_each(|c| *c = false);
            }
            if crate::widgets::text_button(ui, "importAll", crate::i18n::tr("Check All"), false).clicked() {
                for i in 0..n {
                    d.checked[i] = d.importable(i);
                }
            }
        });
    });
    // candidate grid
    let cell = 116.0;
    let avail = ui.available_width();
    let cols = ((avail + 6.0) / (cell + 6.0)).floor().max(1.0) as usize;
    let rows = n.div_ceil(cols);
    egui::ScrollArea::vertical().id_salt("import-grid").max_height(330.0).auto_shrink([false, true]).show_viewport(ui, |ui, viewport| {
        let (area, _) = ui.allocate_exact_size(vec2(avail, rows as f32 * (cell + 26.0)), Sense::hover());
        let row_height = cell + 26.0;
        let first_row = (viewport.min.y / row_height).floor().max(0.0) as usize;
        let last_row = (viewport.max.y / row_height).ceil().max(0.0) as usize;
        for i in first_row.saturating_mul(cols)..n.min(last_row.saturating_add(1).saturating_mul(cols)) {
            let (c, r) = (i % cols, i / cols);
            let local = Rect::from_min_size(pos2(c as f32 * (cell + 6.0), r as f32 * (cell + 26.0)), vec2(cell, cell + 20.0));
            if !local.intersects(viewport.expand(cell)) {
                continue;
            }
            let rect = local.translate(area.min.to_vec2());
            candidate_cell(app, ui, d, i, rect);
        }
    });
    ui.add_space(4.0);
    // options
    if !d.sources.is_empty() {
        field(ui, "Source", |ui| {
            // (whether each source is a folder is checked off the UI thread, once)
            let joined = d.sources.join("\n");
            let summary = crate::panels::left::fs_cached(ui, "import-source", &joined, f64::INFINITY, |s| {
                source_summary(&s.split('\n').map(str::to_string).collect::<Vec<_>>())
            })
            .unwrap_or_else(|| format!("{} item{}", d.sources.len(), if d.sources.len() == 1 { "" } else { "s" }));
            let r = ui.label(egui::RichText::new(summary).color(t.text_label)).on_hover_text(joined);
            register(ui.ctx(), "label:importSource", r.rect);
        });
        ui.label(
            egui::RichText::new(crate::i18n::tr("Importing scans the source for photos; it doesn't add the folder to Local (use Local → Browse Folder… to work in a folder without importing)."))
                .color(t.text_dim)
                .small(),
        );
    }
    field(ui, "Transfer", |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if crate::widgets::text_button(ui, "importAdd", crate::i18n::tr("Add in place"), !d.copy)
            .on_hover_text(crate::i18n::tr("Reference the files where they are"))
            .clicked()
        {
            d.copy = false;
            d.move_files = false;
        }
        let can_copy = app.session.library.as_ref().is_some_and(|l| l.on_disk) || app.services.pick_folder.is_some();
        let r =
            ui.add_enabled_ui(can_copy, |ui| crate::widgets::text_button(ui, "importCopy", crate::i18n::tr("Copy"), d.copy && !d.move_files)).inner;
        if r.on_hover_text(crate::i18n::tr("Copy the files (into the library's Originals/, or a folder you choose)")).clicked() {
            d.copy = true;
            d.move_files = false;
        }
        let r =
            ui.add_enabled_ui(can_copy, |ui| crate::widgets::text_button(ui, "importMove", crate::i18n::tr("Move"), d.copy && d.move_files)).inner;
        if r.on_hover_text(crate::i18n::tr("Move the files (into the library's Originals/, or a folder you choose), removing them from the source"))
            .clicked()
        {
            d.copy = true;
            d.move_files = true;
        }
    });
    let r = ui.label(
        egui::RichText::new(match (d.copy, d.move_files) {
            (true, false) => {
                "Copy: the files are copied to the folder below and the library uses the copies; the originals are left as they are."
            }
            (true, true) => {
                "Move: the files are moved to the folder below; each original (and its XMP sidecar) is removed from the source only after its copy is verified. Duplicates and files that fail stay where they are."
            }
            _ => "Add in place: the library references the files where they are; nothing is copied or moved.",
        })
        .color(if d.copy && d.move_files { t.caution } else { t.text_dim })
        .small(),
    );
    register(ui.ctx(), "label:importModeHelp", r.rect);
    if d.copy {
        field(ui, if d.move_files { "Move to" } else { "Copy to" }, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let shown = if d.destination.trim().is_empty() { "Library Originals".to_string() } else { d.destination.clone() };
            ui.label(egui::RichText::new(shown).color(Tokens::get(ui.ctx()).text_label));
            if app.services.pick_folder.is_some()
                && crate::widgets::text_button(ui, "importDest", crate::i18n::tr("Choose…"), false).clicked()
                && let Some(f) = app.services.pick_folder.as_mut().and_then(|f| f())
            {
                d.destination = f;
            }
            if !d.destination.is_empty() && crate::widgets::text_button(ui, "importDestReset", crate::i18n::tr("Library"), false).clicked() {
                d.destination.clear();
            }
        });
        field(ui, "Folders", |ui| {
            let opts = [
                ("date", "By day (YYYY/YYYY-MM-DD)"),
                ("month", "By month (YYYY/YYYY-MM)"),
                ("flat", "Into one folder"),
                ("custom", "Custom template…"),
            ];
            let key = if d.organize.is_empty() { "date".to_string() } else { d.organize.clone() };
            let cur = opts.iter().find(|o| o.0 == key).map_or(opts[0].1, |o| o.1);
            let combo = egui::ComboBox::from_id_salt("import-organize").selected_text(cur).show_ui(ui, |ui| {
                for (k, label) in opts {
                    let r = ui.selectable_label(key == k, label);
                    register(ui.ctx(), format!("button:importOrganize-{k}"), r.rect);
                    if r.clicked() {
                        d.organize = k.to_string();
                        if k == "custom" && d.folder_template.trim().is_empty() {
                            d.folder_template = DEFAULT_FOLDER_TEMPLATE.into();
                        }
                    }
                }
            });
            register(ui.ctx(), "combo:importOrganize", combo.response.rect);
        });
        if d.organize == "custom" {
            let folders_id = egui::Id::new("import-folder-template");
            let tags_open = field(ui, "Template", |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let w = (ui.available_width() - 50.0).max(80.0);
                let r = ui.add(egui::TextEdit::singleline(&mut d.folder_template).id(folders_id).hint_text(DEFAULT_FOLDER_TEMPLATE).desired_width(w));
                register(ui.ctx(), "field:importFolderTemplate", r.rect);
                tag_toggle(ui, "importFolders")
            });
            if tags_open {
                tag_help(ui, "importFolders", &mut d.folder_template, folders_id);
            }
            let t = Tokens::get(ui.ctx());
            match lightcraft_engine::rename::folder_template_error(&d.folder_template) {
                Some(e) => {
                    ui.label(egui::RichText::new(e).color(t.caution));
                }
                None => unknown_tags_warning(ui, &d.folder_template),
            }
            ui.label(
                egui::RichText::new(crate::i18n::tr(
                    "Each / starts a folder level; tags are filled in per photo (a level with missing metadata is \"unknown\").",
                ))
                .color(t.text_dim)
                .small(),
            );
        }
        if !d.move_files {
            field(ui, "Raw files", |ui| {
                let r = ui.checkbox(&mut d.dng, crate::i18n::tr("Copy as DNG"));
                register(ui.ctx(), "check:importDng", r.rect);
            });
        }
        let rename_id = egui::Id::new("import-rename");
        let tags_open = field(ui, "Rename", |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let w = (ui.available_width() - 50.0).max(80.0);
            let r = ui.add(egui::TextEdit::singleline(&mut d.rename).id(rename_id).hint_text("keep names — or e.g. {date}_{seq:3}").desired_width(w));
            register(ui.ctx(), "field:importRename", r.rect);
            tag_toggle(ui, "importRename")
        });
        if tags_open {
            tag_help(ui, "importRename", &mut d.rename, rename_id);
        }
        unknown_tags_warning(ui, &d.rename);
        if let Some(example) = example_destination(app, d) {
            let t = Tokens::get(ui.ctx());
            let r = ui.label(egui::RichText::new(crate::i18n::tr_format!("Example: {example}", example = example)).color(t.text_dim));
            register(ui.ctx(), "label:importExample", r.rect);
            ui.label(
                egui::RichText::new(crate::i18n::tr("Folders and {date} use the capture time; a photo without one uses today's date."))
                    .color(t.text_dim)
                    .small(),
            );
        }
    }
    let albums: Vec<(u64, String)> = {
        let mut v: Vec<(u64, String)> =
            app.session.catalog.albums().filter(|a| !a.folder && !a.is_smart()).map(|a| (a.id.0, a.name.clone())).collect();
        v.sort_by_key(|(_, n)| n.to_lowercase());
        v
    };
    field(ui, "Album", |ui| {
        let cur = match d.album {
            Some(a) => albums.iter().find(|x| x.0 == a).map(|x| x.1.clone()).unwrap_or_default(),
            None if !d.new_album.is_empty() => "New album".into(),
            None => "None".into(),
        };
        egui::ComboBox::from_id_salt("import-album").selected_text(cur).show_ui(ui, |ui| {
            if ui.selectable_label(d.album.is_none() && d.new_album.is_empty(), crate::i18n::tr("None")).clicked() {
                d.album = None;
                d.new_album.clear();
            }
            if ui.selectable_label(d.album.is_none() && !d.new_album.is_empty(), crate::i18n::tr("New album…")).clicked() {
                d.album = None;
                if d.new_album.is_empty() {
                    d.new_album = "Imported Photos".into();
                }
            }
            for (id, name) in &albums {
                if ui.selectable_label(d.album == Some(*id), name).clicked() {
                    d.album = Some(*id);
                    d.new_album.clear();
                }
            }
        });
        if d.album.is_none() && !d.new_album.is_empty() {
            let r = ui.add(egui::TextEdit::singleline(&mut d.new_album).desired_width(f32::INFINITY));
            register(ui.ctx(), "field:importAlbumName", r.rect);
        }
    });
    field(ui, "Preset", |ui| {
        let cur = app.session.presets.iter().find(|p| p.id == d.preset).map(|p| p.name.clone()).unwrap_or_else(|| "None".into());
        egui::ComboBox::from_id_salt("import-preset").selected_text(cur).height(300.0).show_ui(ui, |ui| {
            if ui.selectable_label(d.preset.is_empty(), crate::i18n::tr("None")).clicked() {
                d.preset.clear();
            }
            for p in &app.session.presets {
                if ui.selectable_label(d.preset == p.id, format!("{} — {}", p.group, p.name)).clicked() {
                    d.preset = p.id.clone();
                }
            }
        });
    });
    let r = ui.checkbox(&mut d.auto_without_xmp, "Apply Auto to photos without XMP");
    register(ui.ctx(), "check:importAutoWithoutXmp", r.rect);
    if !app.session.metadata_presets.is_empty() {
        field(ui, "Metadata", |ui| {
            let cur = if d.metadata_preset.is_empty() { "None".to_string() } else { d.metadata_preset.clone() };
            egui::ComboBox::from_id_salt("import-metadata").selected_text(cur).show_ui(ui, |ui| {
                if ui.selectable_label(d.metadata_preset.is_empty(), crate::i18n::tr("None")).clicked() {
                    d.metadata_preset.clear();
                }
                for m in &app.session.metadata_presets {
                    if ui.selectable_label(d.metadata_preset == m.name, &m.name).clicked() {
                        d.metadata_preset = m.name.clone();
                    }
                }
            });
        });
    }
    field(ui, "Keywords", |ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut d.keywords).hint_text(crate::i18n::tr("comma, separated")).desired_width(f32::INFINITY));
        register(ui.ctx(), "field:importKeywords", r.rect);
    });
}

fn candidate_cell(app: &mut LightcraftApp, ui: &mut egui::Ui, d: &mut ImportDialog, i: usize, rect: Rect) {
    let t = Tokens::get(ui.ctx());
    let c = d.candidates[i].clone();
    let ok = d.importable(i);
    let img = Rect::from_min_size(rect.min, vec2(rect.width(), rect.width()));
    let resp = ui.interact(img, egui::Id::new(("import-cell", i)), Sense::click());
    register(ui.ctx(), format!("import:{i}"), img);
    let p = ui.painter();
    p.rect_filled(img, 3.0, Color32::from_gray(30));
    // thumbnail (background job; the grid only asks for the cells in view)
    let slot = Slot::Import(i as u32);
    if !app.renderer.textures.contains_key(&slot)
        && let Some(job) = app.session.candidate_thumb_job(&c, 192, i as u64)
    {
        app.renderer.request_quick(slot, job, 4);
    }
    if let Some(tex) = app.renderer.textures.get(&slot) {
        let [tw, th] = tex.size;
        let s = ((img.width() - 8.0) / tw as f32).min((img.height() - 8.0) / th as f32);
        let fit = Rect::from_center_size(img.center(), vec2(tw as f32 * s, th as f32 * s));
        let tint = if ok { Color32::WHITE } else { Color32::from_gray(110) };
        p.image(tex.tex.id(), fit, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
    }
    let on = d.checked[i] && ok;
    if on {
        p.rect_stroke(img, 3.0, Stroke::new(2.0, t.accent), StrokeKind::Inside);
    }
    // checkbox
    let cb = Rect::from_min_size(img.min + vec2(6.0, 6.0), vec2(16.0, 16.0));
    p.rect(cb, 3.0, if on { t.accent } else { Color32::from_black_alpha(150) }, Stroke::new(1.0, Color32::from_gray(200)), StrokeKind::Inside);
    if on {
        p.line_segment([cb.left_center() + vec2(3.5, 0.5), cb.center_bottom() + vec2(-1.0, -4.0)], Stroke::new(2.0, Color32::WHITE));
        p.line_segment([cb.center_bottom() + vec2(-1.0, -4.0), cb.right_top() + vec2(-3.5, 4.0)], Stroke::new(2.0, Color32::WHITE));
    }
    let badge = match (&c.duplicate, &c.error) {
        (Some(r), _) if r == "path" => Some("In library"),
        (Some(_), _) => Some("Duplicate"),
        (None, Some(_)) => Some("Unreadable"),
        _ => None,
    };
    if let Some(b) = badge {
        let g = p.layout_no_wrap(b.to_string(), t.semibold(10.0), Color32::WHITE);
        let br = Rect::from_min_size(pos2(img.right() - g.size().x - 12.0, img.top() + 6.0), g.size() + vec2(8.0, 4.0));
        p.rect_filled(br, 3.0, Color32::from_rgba_unmultiplied(170, 60, 50, 220));
        p.galley(br.min + vec2(4.0, 2.0), g, Color32::WHITE);
    }
    let name = if c.name.chars().count() > 18 { format!("{}…", c.name.chars().take(17).collect::<String>()) } else { c.name.clone() };
    p.text(pos2(rect.left() + 2.0, img.bottom() + 9.0), Align2::LEFT_CENTER, name, t.font(10.5), if ok { t.text_label } else { t.text_dim });
    let tip = format!(
        "{}\n{} × {} · {} · {:.1} MB{}",
        c.path,
        c.width,
        c.height,
        c.format,
        c.file_size as f64 / 1e6,
        c.captured.as_deref().map(|d| format!("\n{}", d.replace('T', " "))).unwrap_or_default()
    );
    let resp = resp.on_hover_text(tip);
    if resp.clicked() && ok {
        d.checked[i] = !d.checked[i];
    }
}

/// The "Tags" toggle beside a template field (`button:<key>Tags`); returns whether the tag help
/// ([`tag_help`]) is open.
pub(crate) fn tag_toggle(ui: &mut egui::Ui, key: &str) -> bool {
    let id = egui::Id::new((key, "tags-open"));
    let mut open = ui.ctx().data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let r = crate::widgets::text_button(ui, &format!("{key}Tags"), crate::i18n::tr("Tags"), open)
        .on_hover_text(crate::i18n::tr("Show the template tags; click one to insert it"));
    if r.clicked() {
        open = !open;
        ui.ctx().data_mut(|d| d.insert_temp(id, open));
    }
    open
}

/// "Unknown tag {x} stays as typed" under a template field, when it has one.
pub(crate) fn unknown_tags_warning(ui: &mut egui::Ui, template: &str) {
    let unknown = lightcraft_engine::rename::unknown_tokens(template);
    if !unknown.is_empty() {
        let s = if unknown.len() == 1 { "" } else { "s" };
        let text = format!("Unknown tag{s} {} stay{} as typed (see Tags)", unknown.join(" "), if unknown.len() == 1 { "s" } else { "" });
        ui.label(egui::RichText::new(text).color(Tokens::get(ui.ctx()).caution));
    }
}

/// Insert `tag` into `text` at the text cursor of the field `edit_id` (replacing its selection);
/// appended when the field never had a cursor. The cursor ends up after the tag.
pub(crate) fn insert_at_cursor(ctx: &egui::Context, edit_id: egui::Id, text: &mut String, tag: &str) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = egui::text_edit::TextEditState::load(ctx, edit_id).unwrap_or_default();
    let n = text.chars().count();
    let (a, b) = state.cursor.char_range().map_or((n, n), |r| {
        let (x, y) = (r.primary.index.0.min(n), r.secondary.index.0.min(n));
        (x.min(y), x.max(y))
    });
    let byte = |c: usize| text.char_indices().nth(c).map_or(text.len(), |(i, _)| i);
    let (ba, bb) = (byte(a), byte(b));
    text.replace_range(ba..bb, tag);
    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(a + tag.chars().count()))));
    state.store(ctx, edit_id);
    ctx.memory_mut(|m| m.request_focus(edit_id));
}

/// The template tag help: every tag (from the engine's one list, `rename::TOKENS`) with its
/// meaning and an example, the `{date:…}` directives and how templates behave. Clicking a tag
/// (`button:<key>Tag-<i>`) inserts it at the cursor of the field `edit_id`.
pub(crate) fn tag_help(ui: &mut egui::Ui, key: &str, text: &mut String, edit_id: egui::Id) {
    use lightcraft_engine::rename::{DATE_DIRECTIVES, TEMPLATE_NOTES, TOKENS, token_example};
    let t = Tokens::get(ui.ctx());
    egui::Frame::new().fill(t.inset).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.label(
            egui::RichText::new(crate::i18n::tr("Click a tag to insert it. Examples are for IMG_0042.CR3 taken 14 Jan 2026, 05:58:48."))
                .color(t.text_dim)
                .small(),
        );
        egui::ScrollArea::vertical().id_salt((key, "tags")).max_height(170.0).show(ui, |ui| {
            egui::Grid::new((key, "tag-grid")).num_columns(3).spacing([10.0, 2.0]).show(ui, |ui| {
                for (i, tok) in TOKENS.iter().enumerate() {
                    let r = ui.add(egui::Button::new(egui::RichText::new(tok.tag).monospace().color(t.text)).small()).on_hover_text(
                        if tok.aliases.is_empty() {
                            "Insert at the cursor".to_string()
                        } else {
                            format!("Insert at the cursor (also written {})", tok.aliases.join(", "))
                        },
                    );
                    register(ui.ctx(), format!("button:{key}Tag-{i}"), r.rect);
                    if r.clicked() {
                        insert_at_cursor(ui.ctx(), edit_id, text, tok.tag);
                    }
                    ui.label(egui::RichText::new(tok.meaning).color(t.text_label));
                    ui.label(egui::RichText::new(token_example(tok.tag)).monospace().color(t.text_dim));
                    ui.end_row();
                }
            });
            let directives: Vec<String> = DATE_DIRECTIVES.iter().map(|(d, m)| format!("{d} {m}")).collect();
            ui.label(egui::RichText::new(format!("{{date:…}} directives: {}", directives.join(" · "))).color(t.text_dim).small());
            for n in TEMPLATE_NOTES {
                ui.label(egui::RichText::new(format!("• {n}")).color(t.text_dim).small());
            }
        });
    });
}

/// The folder template the Custom choice starts with (`2026/20260114/`).
pub const DEFAULT_FOLDER_TEMPLATE: &str = "{date:%Y}/{date:%Y%m%d}";

/// Where the first selected photo would be copied to (destination, folders, name), for the
/// dialog's example line; `None` without a photo or with an unusable folder template.
pub fn example_destination(app: &LightcraftApp, d: &ImportDialog) -> Option<String> {
    let c = d.candidates.iter().zip(&d.checked).find(|(c, on)| **on && c.duplicate.is_none() && c.error.is_none()).map(|(c, _)| c)?;
    let organize = match d.organize_param().ok()? {
        Some(o) => lightcraft_engine::import::Organize::parse(&o)?,
        None => Default::default(),
    };
    let mut q = lightcraft_catalog::Photo::new(
        lightcraft_catalog::PhotoId(0),
        lightcraft_catalog::Source::File { path: c.path.clone() },
        &c.name,
        &c.format,
        0,
        0,
        // a photo without a capture time is dated by the import (now)
        &(app.session.clock)(),
    );
    q.captured = c.captured.clone();
    // the probe's metadata, so {camera}, {title}… preview as they will import
    if let Some(info) = app.session.import_probes.get(&c.path) {
        q.meta = info.meta.clone();
    }
    let name = if d.rename.trim().is_empty() { c.name.clone() } else { lightcraft_engine::rename::expand(d.rename.trim(), &q, 1) };
    let root = if d.destination.trim().is_empty() {
        app.session.library.as_ref().map_or_else(|| "Originals".to_string(), |l| l.dir.join("Originals").to_string_lossy().to_string())
    } else {
        d.destination.trim().trim_end_matches(['/', '\\']).to_string()
    };
    let sep = std::path::MAIN_SEPARATOR_STR;
    let mut parts = vec![root];
    parts.extend(organize.folders(&q));
    parts.push(name);
    Some(parts.join(sep))
}

/// The import source in a few words: a folder's name, a file's name, or "N files and folders".
pub fn source_summary(sources: &[String]) -> String {
    let name = |p: &str| std::path::Path::new(p).file_name().map_or_else(|| p.to_string(), |n| n.to_string_lossy().to_string());
    match sources {
        [one] if std::path::Path::new(one).is_dir() => format!("Folder “{}” (and its subfolders)", name(one)),
        [one] => name(one),
        many => {
            let folders = many.iter().filter(|p| std::path::Path::new(p.as_str()).is_dir()).count();
            match folders {
                0 => crate::i18n::tr_format!("{} files", many.len()),
                f if f == many.len() => crate::i18n::tr_format!("{f} folders (and their subfolders)", f = f),
                f => crate::i18n::tr_format!("{} files and {f} folder{}", many.len() - f, if f == 1 { "" } else { "s" }, f = f),
            }
        }
    }
}

/// A labelled row (fixed label column).
fn field<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let t = Tokens::get(ui.ctx());
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(vec2(78.0, 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(78.0);
            ui.label(egui::RichText::new(label).color(t.text_label));
        });
        add(ui)
    })
    .inner
}
