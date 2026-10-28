use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use egui::epaint::Vertex;
use egui_plot::{Plot, PlotMemory, PlotPoint};
use std::collections::{HashMap, HashSet};

use crate::{Color, DrawCommand, UiEvent};

#[derive(Clone, Copy)]
struct BBox {
    min: [f64; 2],
    max: [f64; 2],
}

impl BBox {
    fn from_points(pts: &[[f64; 2]]) -> Self {
        let mut min = [f64::INFINITY; 2];
        let mut max = [f64::NEG_INFINITY; 2];
        for p in pts {
            min[0] = min[0].min(p[0]);
            min[1] = min[1].min(p[1]);
            max[0] = max[0].max(p[0]);
            max[1] = max[1].max(p[1]);
        }
        Self { min, max }
    }

    fn intersects(&self, other: &BBox) -> bool {
        self.max[0] >= other.min[0]
            && self.min[0] <= other.max[0]
            && self.max[1] >= other.min[1]
            && self.min[1] <= other.max[1]
    }

    fn contains(&self, p: [f64; 2]) -> bool {
        p[0] >= self.min[0] && p[0] <= self.max[0] && p[1] >= self.min[1] && p[1] <= self.max[1]
    }
}

#[derive(Clone)]
enum Shape {
    Polygon {
        vertices: Vec<[f64; 2]>,
        triangles: Vec<[usize; 3]>,
        fill: Option<egui::Color32>,
        bbox: BBox,
    },
    Line {
        from: [f64; 2],
        to: [f64; 2],
        color: egui::Color32,
    },
    Points {
        positions: Vec<[f64; 2]>,
        radius: f32,
        color: egui::Color32,
    },
    Text {
        pos: [f64; 2],
        text: String,
        color: egui::Color32,
    },
}

#[derive(Clone, Copy, PartialEq)]
struct TransformKey {
    frame: egui::Rect,
    bounds: [f64; 4],
}

struct BuiltLayer {
    key: TransformKey,
    mesh: egui::Mesh,
    labels: Vec<(egui::Pos2, String, egui::Color32)>,
}

pub struct VisApp {
    receiver: Receiver<DrawCommand>,
    events: Sender<UiEvent>,
    layers: HashMap<String, Vec<Shape>>,
    layer_visible: HashMap<String, bool>,
    layer_order: Vec<String>,
    dirty: HashSet<String>,
    built: HashMap<String, BuiltLayer>,
    data_bounds: Option<[[f64; 2]; 2]>,
    last_hover: Option<egui::Pos2>,
    status: String,
    show_grid: bool,
    crosshair: bool,
    controls: Vec<(String, f64)>,
    action_label: Option<String>,
    modes: Vec<String>,
    mode_selected: usize,
    progress: Vec<(String, f32)>,
    progress_history: HashMap<String, Vec<f32>>,
}

impl VisApp {
    pub fn new(receiver: Receiver<DrawCommand>, events: Sender<UiEvent>) -> Self {
        Self {
            receiver,
            events,
            layers: HashMap::new(),
            layer_visible: HashMap::new(),
            layer_order: Vec::new(),
            dirty: HashSet::new(),
            built: HashMap::new(),
            data_bounds: None,
            last_hover: None,
            status: String::new(),
            show_grid: false,
            crosshair: false,
            controls: Vec::new(),
            action_label: None,
            modes: Vec::new(),
            mode_selected: 0,
            progress: Vec::new(),
            progress_history: HashMap::new(),
        }
    }

    fn drain_commands(&mut self) -> bool {
        let mut received = false;
        while let Ok(cmd) = self.receiver.try_recv() {
            received = true;
            match cmd {
                DrawCommand::ClearLayer { layer } => {
                    self.ensure_layer(&layer);
                    self.layers.get_mut(&layer).unwrap().clear();
                    self.dirty.insert(layer);
                }
                DrawCommand::Status { text } => {
                    self.status = text;
                }
                DrawCommand::SetControls { fields, button } => {
                    if fields.is_empty() && button.is_empty() {
                        self.controls.clear();
                        self.action_label = None;
                    } else {
                        self.controls = fields;
                        self.action_label = Some(button);
                    }
                }
                DrawCommand::SetModes { options, selected } => {
                    self.modes = options;
                    self.mode_selected = selected;
                }
                DrawCommand::SetProgress { items } => {
                    if items.iter().any(|(label, value)| {
                        metric_key(label) == "generation" && *value <= f32::EPSILON
                    }) {
                        self.progress_history.clear();
                    }
                    for (label, value) in &items {
                        let history = self
                            .progress_history
                            .entry(metric_key(label).to_string())
                            .or_default();
                        history.push(value.clamp(0.0, 1.0));
                        if history.len() > 512 {
                            history.drain(0..history.len() - 512);
                        }
                    }
                    self.progress = items;
                }
                DrawCommand::SetLayerVisible { layer, visible } => {
                    if let Some(v) = self.layer_visible.get_mut(&layer) {
                        *v = visible;
                    }
                }
                DrawCommand::HideAllLayers => {
                    for v in self.layer_visible.values_mut() {
                        *v = false;
                    }
                }
                DrawCommand::Polygon {
                    vertices,
                    triangles,
                    fill,
                    layer,
                } => {
                    self.ensure_layer(&layer);
                    let verts: Vec<[f64; 2]> = vertices.iter().map(|p| [p.x, p.y]).collect();
                    self.expand_bounds(&verts);
                    let bbox = BBox::from_points(&verts);
                    self.layers.get_mut(&layer).unwrap().push(Shape::Polygon {
                        vertices: verts,
                        triangles,
                        fill: fill.map(|c| to_color32(&c)),
                        bbox,
                    });
                    self.dirty.insert(layer);
                }
                DrawCommand::Line {
                    from,
                    to,
                    color,
                    layer,
                } => {
                    self.ensure_layer(&layer);
                    self.layers.get_mut(&layer).unwrap().push(Shape::Line {
                        from: [from.x, from.y],
                        to: [to.x, to.y],
                        color: to_color32(&color),
                    });
                    self.dirty.insert(layer);
                }
                DrawCommand::Points {
                    positions,
                    radius,
                    color,
                    layer,
                } => {
                    self.ensure_layer(&layer);
                    let pts: Vec<[f64; 2]> = positions.iter().map(|p| [p.x, p.y]).collect();
                    self.layers.get_mut(&layer).unwrap().push(Shape::Points {
                        positions: pts,
                        radius,
                        color: to_color32(&color),
                    });
                    self.dirty.insert(layer);
                }
                DrawCommand::Text {
                    pos,
                    text,
                    color,
                    layer,
                } => {
                    self.ensure_layer(&layer);
                    self.layers.get_mut(&layer).unwrap().push(Shape::Text {
                        pos: [pos.x, pos.y],
                        text,
                        color: to_color32(&color),
                    });
                    self.dirty.insert(layer);
                }
            }
        }
        received
    }

    fn expand_bounds(&mut self, pts: &[[f64; 2]]) {
        for p in pts {
            match &mut self.data_bounds {
                None => self.data_bounds = Some([*p, *p]),
                Some([min, max]) => {
                    min[0] = min[0].min(p[0]);
                    min[1] = min[1].min(p[1]);
                    max[0] = max[0].max(p[0]);
                    max[1] = max[1].max(p[1]);
                }
            }
        }
    }

    fn ensure_layer(&mut self, layer: &str) {
        if !self.layers.contains_key(layer) {
            self.layers.insert(layer.to_string(), Vec::new());
            self.layer_visible.insert(layer.to_string(), true);
            self.layer_order.push(layer.to_string());
        }
    }

    fn build_layer(
        shapes: &[Shape],
        key: TransformKey,
        transform: &egui_plot::PlotTransform,
        pixels_per_point: f32,
    ) -> BuiltLayer {
        let white_uv = egui::pos2(0.0, 0.0);
        let line_half_width = 0.5 * pixels_per_point;
        let mut mesh = egui::Mesh::default();
        let mut labels = Vec::new();

        let view_bbox = BBox {
            min: [key.bounds[0], key.bounds[1]],
            max: [key.bounds[2], key.bounds[3]],
        };
        let to_screen = |p: [f64; 2]| transform.position_from_point(&PlotPoint::new(p[0], p[1]));

        for shape in shapes {
            match shape {
                Shape::Polygon {
                    vertices,
                    triangles,
                    fill,
                    bbox,
                } => {
                    if !bbox.intersects(&view_bbox) {
                        continue;
                    }
                    let screen_verts: Vec<egui::Pos2> =
                        vertices.iter().map(|v| to_screen(*v)).collect();
                    if let Some(f) = fill {
                        for tri in triangles {
                            let idx = mesh.vertices.len() as u32;
                            for &i in tri {
                                mesh.vertices.push(Vertex {
                                    pos: screen_verts[i],
                                    uv: white_uv,
                                    color: *f,
                                });
                            }
                            mesh.indices.extend_from_slice(&[idx, idx + 1, idx + 2]);
                        }
                    }
                }
                Shape::Line { from, to, color } => {
                    push_line_quad(
                        &mut mesh,
                        to_screen(*from),
                        to_screen(*to),
                        *color,
                        white_uv,
                        line_half_width,
                    );
                }
                Shape::Points {
                    positions,
                    radius,
                    color,
                } => {
                    for p in positions {
                        if view_bbox.contains(*p) {
                            push_dot(&mut mesh, to_screen(*p), *radius, *color, white_uv);
                        }
                    }
                }
                Shape::Text { pos, text, color } => {
                    if view_bbox.contains(*pos) {
                        labels.push((to_screen(*pos), text.clone(), *color));
                    }
                }
            }
        }

        BuiltLayer { key, mesh, labels }
    }
}

impl eframe::App for VisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let new_data = self.drain_commands();
        if new_data {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::SidePanel::left("controls")
            .resizable(true)
            .default_width(230.0)
            .show(ctx, |ui| {
                if !self.modes.is_empty() {
                    ui.heading("Mode");
                    ui.separator();
                    ui.horizontal(|ui| {
                        for (i, option) in self.modes.iter().enumerate() {
                            if ui.radio_value(&mut self.mode_selected, i, option).clicked() {
                                let _ = self.events.try_send(UiEvent::Mode(i));
                            }
                        }
                    });
                    ui.add_space(8.0);
                }
                if let Some(label) = &self.action_label {
                    ui.heading("Run");
                    ui.separator();
                    for (name, value) in &mut self.controls {
                        ui.horizontal(|ui| {
                            ui.label(name.as_str());
                            ui.add(egui::DragValue::new(value).speed(0.05));
                        });
                    }
                    if ui.button(label).clicked() {
                        let values = self.controls.iter().map(|(_, v)| *v).collect();
                        let _ = self.events.try_send(UiEvent::Action(values));
                    }
                    ui.add_space(8.0);
                }
                ui.heading("View");
                ui.separator();
                ui.checkbox(&mut self.show_grid, "grid");
                ui.checkbox(&mut self.crosshair, "crosshair");
                ui.add_space(8.0);
                ui.heading("Layers");
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("layer_scroll")
                    .max_height(ui.available_height())
                    .show(ui, |ui| {
                        for name in &self.layer_order {
                            let visible = self.layer_visible.get_mut(name).unwrap();
                            let count = self.layers.get(name).map_or(0, |s| s.len());
                            ui.checkbox(visible, format!("{name} ({count})"));
                        }
                    });
            });

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    ui.add_enabled(false, egui::Button::new("Import instance…"));
                    ui.add_enabled(false, egui::Button::new("Import solution…"));
                    ui.add_enabled(false, egui::Button::new("Export solution…"));
                    ui.separator();
                    ui.label(egui::RichText::new("CLI-backed for now").small().italics());
                });
                ui.menu_button("Run", |ui| {
                    ui.add_enabled(false, egui::Button::new("Start GA…"));
                    ui.add_enabled(false, egui::Button::new("Cancel run"));
                    ui.add_enabled(false, egui::Button::new("Infinite search"));
                    ui.separator();
                    ui.label(
                        egui::RichText::new("typed run panel planned")
                            .small()
                            .italics(),
                    );
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Show all layers").clicked() {
                        for visible in self.layer_visible.values_mut() {
                            *visible = true;
                        }
                        ui.close_menu();
                    }
                    if ui.button("Hide all layers").clicked() {
                        for visible in self.layer_visible.values_mut() {
                            *visible = false;
                        }
                        ui.close_menu();
                    }
                });
                ui.separator();
                ui.strong("gisard workbench");
                ui.separator();
                ui.label("wheel zooms, drag pans, click to inspect");
            });
        });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.heading("Inspector");
                ui.separator();
                if !self.progress.is_empty() {
                    ui.heading("Progress");
                    for (label, value) in &self.progress {
                        ui.label(label);
                        ui.add(egui::ProgressBar::new(value.clamp(0.0, 1.0)).show_percentage());
                    }
                    if self.progress_history.values().any(|h| h.len() > 1) {
                        ui.add_space(4.0);
                        ui.heading("Metric history");
                        for (label, _) in &self.progress {
                            let key = metric_key(label);
                            if let Some(values) = self.progress_history.get(key)
                                && values.len() > 1
                            {
                                progress_sparkline(ui, key, values);
                            }
                        }
                    }
                    ui.separator();
                }
                egui::ScrollArea::vertical()
                    .id_salt("inspector_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.status.is_empty() {
                            ui.label(
                                egui::RichText::new("Hover or click a building/antenna.").italics(),
                            );
                        } else {
                            ui.label(egui::RichText::new(&self.status).monospace().size(11.0));
                        }
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let plot_id = egui::Id::new("main_plot");
            let mut plot = Plot::new("main_plot")
                .id(plot_id)
                .data_aspect(1.0)
                .allow_zoom(false)
                .allow_scroll(false)
                .show_grid(self.show_grid)
                .show_axes(false)
                .show_x(self.crosshair)
                .show_y(self.crosshair);
            if let Some([min, max]) = &self.data_bounds {
                plot = plot
                    .include_x(min[0])
                    .include_x(max[0])
                    .include_y(min[1])
                    .include_y(max[1]);
            }
            let plot_response = plot.show(ui, |_plot_ui| {});

            if plot_response.response.contains_pointer() {
                let scroll_y = ctx.input(|i| i.raw_scroll_delta.y);
                if scroll_y.abs() > f32::EPSILON
                    && let Some(hover_pos) = ctx.input(|i| i.pointer.hover_pos())
                    && let Some(mut memory) = PlotMemory::load(ctx, plot_id)
                {
                    let zoom = (scroll_y * 0.002).exp().clamp(0.2, 5.0);
                    let mut transform = memory.transform();
                    transform.zoom(egui::Vec2::splat(zoom), hover_pos);
                    memory.set_transform(transform);
                    memory.auto_bounds = false.into();
                    memory.store(ctx, plot_id);
                    ctx.request_repaint();
                }
            }

            if let Some(pos) = plot_response.response.hover_pos() {
                let clicked = plot_response.response.clicked();
                if clicked || self.last_hover != Some(pos) {
                    self.last_hover = Some(pos);
                    let world = plot_response.transform.value_from_position(pos);
                    let p = framework::Point::new(world.x, world.y);
                    let event = if clicked {
                        UiEvent::Click(p)
                    } else {
                        UiEvent::Hover(p)
                    };
                    let _ = self.events.try_send(event);
                }
            } else {
                self.last_hover = None;
            }

            let transform = &plot_response.transform;
            let bounds = transform.bounds();
            let current_key = TransformKey {
                frame: plot_response.response.rect,
                bounds: [
                    bounds.min()[0],
                    bounds.min()[1],
                    bounds.max()[0],
                    bounds.max()[1],
                ],
            };

            for name in &self.layer_order {
                if !self.layer_visible.get(name).copied().unwrap_or(true) {
                    continue;
                }
                let stale = self.dirty.contains(name)
                    || self.built.get(name).is_none_or(|b| b.key != current_key);
                if stale {
                    let built = Self::build_layer(
                        &self.layers[name],
                        current_key,
                        transform,
                        ctx.pixels_per_point(),
                    );
                    self.built.insert(name.clone(), built);
                    self.dirty.remove(name);
                }
            }

            let clip_rect = plot_response.response.rect;
            let mut painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("scene"),
            ));
            painter.set_clip_rect(clip_rect);
            for name in &self.layer_order {
                if !self.layer_visible.get(name).copied().unwrap_or(true) {
                    continue;
                }
                let Some(built) = self.built.get(name) else {
                    continue;
                };
                if !built.mesh.is_empty() {
                    painter.add(egui::Shape::mesh(built.mesh.clone()));
                }
                for (pos, text, color) in &built.labels {
                    painter.text(
                        *pos,
                        egui::Align2::CENTER_BOTTOM,
                        text,
                        egui::FontId::proportional(12.0),
                        *color,
                    );
                }
            }
        });
    }
}

fn push_line_quad(
    mesh: &mut egui::Mesh,
    a: egui::Pos2,
    b: egui::Pos2,
    color: egui::Color32,
    uv: egui::Pos2,
    half_width: f32,
) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.001 {
        return;
    }
    let nx = -dy / len * half_width;
    let ny = dx / len * half_width;
    let n = egui::vec2(nx, ny);

    let idx = mesh.vertices.len() as u32;
    mesh.vertices.push(Vertex {
        pos: a - n,
        uv,
        color,
    });
    mesh.vertices.push(Vertex {
        pos: a + n,
        uv,
        color,
    });
    mesh.vertices.push(Vertex {
        pos: b + n,
        uv,
        color,
    });
    mesh.vertices.push(Vertex {
        pos: b - n,
        uv,
        color,
    });
    mesh.indices
        .extend_from_slice(&[idx, idx + 1, idx + 2, idx, idx + 2, idx + 3]);
}

fn push_dot(
    mesh: &mut egui::Mesh,
    center: egui::Pos2,
    radius: f32,
    color: egui::Color32,
    uv: egui::Pos2,
) {
    const N: usize = 8;
    let c = mesh.vertices.len() as u32;
    mesh.vertices.push(Vertex {
        pos: center,
        uv,
        color,
    });
    for i in 0..N {
        let angle = std::f32::consts::TAU * i as f32 / N as f32;
        mesh.vertices.push(Vertex {
            pos: center + radius * egui::vec2(angle.cos(), angle.sin()),
            uv,
            color,
        });
    }
    for i in 0..N as u32 {
        let next = (i + 1) % N as u32;
        mesh.indices
            .extend_from_slice(&[c, c + 1 + i, c + 1 + next]);
    }
}

fn progress_sparkline(ui: &mut egui::Ui, label: &str, values: &[f32]) {
    let min = values.iter().copied().fold(f32::INFINITY, f32::min);
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let current = values.last().copied().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).small().strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "now {:>5.1}%   min {:>5.1}%   max {:>5.1}%",
                    100.0 * current,
                    100.0 * min,
                    100.0 * max
                ))
                .small()
                .monospace(),
            );
        });
    });
    let width = ui.available_width().max(80.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 54.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    for t in [0.25, 0.5, 0.75] {
        let y = rect.bottom() - 4.0 - (rect.height() - 8.0) * t;
        painter.line_segment(
            [
                egui::pos2(rect.left() + 4.0, y),
                egui::pos2(rect.right() - 4.0, y),
            ],
            egui::Stroke::new(0.6, ui.visuals().weak_text_color().linear_multiply(0.45)),
        );
    }
    if values.len() < 2 {
        return;
    }
    let left = rect.left() + 4.0;
    let right = rect.right() - 4.0;
    let top = rect.top() + 4.0;
    let bottom = rect.bottom() - 4.0;
    let span = (values.len() - 1).max(1) as f32;
    let to_pos = |i: usize, v: f32| {
        egui::pos2(
            left + (right - left) * i as f32 / span,
            bottom - (bottom - top) * v.clamp(0.0, 1.0),
        )
    };
    let stroke = egui::Stroke::new(1.8, egui::Color32::from_rgb(120, 200, 255));
    for (i, pair) in values.windows(2).enumerate() {
        painter.line_segment([to_pos(i, pair[0]), to_pos(i + 1, pair[1])], stroke);
    }
}

fn metric_key(label: &str) -> &str {
    let trimmed = label.trim();
    let end = trimmed
        .char_indices()
        .find_map(|(i, ch)| (ch.is_ascii_digit() || ch == '(').then_some(i))
        .unwrap_or(trimmed.len())
        .min(trimmed.len());
    trimmed[..end].trim_end()
}

fn to_color32(c: &Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.0, c.1, c.2, c.3)
}
