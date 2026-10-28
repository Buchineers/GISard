mod app;

pub use crossbeam_channel::{Receiver, RecvTimeoutError};
use crossbeam_channel::{Sender, unbounded};
use framework::Point;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct Color(pub u8, pub u8, pub u8, pub u8);

impl Color {
    pub const RED: Color = Color(255, 0, 0, 255);
    pub const GREEN: Color = Color(0, 200, 0, 255);
    pub const BLUE: Color = Color(50, 100, 255, 255);
    pub const WHITE: Color = Color(255, 255, 255, 255);
    pub const BLACK: Color = Color(0, 0, 0, 255);
    pub const GRAY: Color = Color(180, 180, 180, 255);
    pub const YELLOW: Color = Color(255, 220, 0, 255);

    pub const SERVICED: Color = Color(120, 200, 120, 255);
    pub const UNSERVICED: Color = Self::GRAY;
    pub const COVERED: Color = Color(0, 160, 0, 255);
    pub const UNCOVERED: Color = Color(220, 50, 50, 255);
    pub const REGION_FILL: Color = Color(80, 160, 255, 28);
    pub const PREVIEW_FILL: Color = Color(120, 200, 255, 60);
}

pub fn coverage_color(progress: f64) -> Color {
    let progress = progress.clamp(0.0, 1.0);
    if progress >= 1.0 {
        return Color::SERVICED;
    }
    let smooth = progress * progress * (3.0 - 2.0 * progress);
    let blend = |a: f64, b: f64, t: f64| (a + (b - a) * t) as u8;
    Color(
        blend(55.0, 255.0, smooth),
        blend(105.0, 190.0, smooth),
        blend(220.0, 45.0, smooth),
        blend(35.0, 165.0, progress),
    )
}

#[derive(Clone, Debug)]
pub enum DrawCommand {
    Polygon {
        vertices: Vec<Point>,
        triangles: Vec<[usize; 3]>,
        fill: Option<Color>,
        layer: String,
    },
    Line {
        from: Point,
        to: Point,
        color: Color,
        layer: String,
    },
    Points {
        positions: Vec<Point>,
        radius: f32,
        color: Color,
        layer: String,
    },
    Text {
        pos: Point,
        text: String,
        color: Color,
        layer: String,
    },
    ClearLayer {
        layer: String,
    },
    Status {
        text: String,
    },
    SetControls {
        fields: Vec<(String, f64)>,
        button: String,
    },
    SetModes {
        options: Vec<String>,
        selected: usize,
    },
    SetProgress {
        items: Vec<(String, f32)>,
    },
    SetLayerVisible {
        layer: String,
        visible: bool,
    },
    HideAllLayers,
}

pub trait Canvas: Send + Sync {
    fn draw(&self, cmd: DrawCommand);
    fn clear(&self, layer: &str);
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Hover(Point),
    Click(Point),
    Action(Vec<f64>),
    Mode(usize),
}

pub struct EguiCanvas {
    sender: Sender<DrawCommand>,
}

impl EguiCanvas {
    pub fn create_interactive() -> (Arc<Self>, Receiver<UiEvent>, impl FnOnce()) {
        let (sender, receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();
        let canvas = Arc::new(Self { sender });
        let run_ui = move || {
            let native_options = eframe::NativeOptions {
                vsync: false,
                ..Default::default()
            };
            eframe::run_native(
                "gisard visualizer",
                native_options,
                Box::new(|_cc| Ok(Box::new(app::VisApp::new(receiver, event_sender)))),
            )
            .expect("failed to run visualizer");
        };
        (canvas, event_receiver, run_ui)
    }
}

impl Canvas for EguiCanvas {
    fn draw(&self, cmd: DrawCommand) {
        let _ = self.sender.send(cmd);
    }

    fn clear(&self, layer: &str) {
        let _ = self.sender.send(DrawCommand::ClearLayer {
            layer: layer.to_string(),
        });
    }
}

pub fn tessellate(vertices: &[Point]) -> Vec<[usize; 3]> {
    if vertices.len() < 3 {
        return Vec::new();
    }
    let flat: Vec<f64> = vertices.iter().flat_map(|p| [p.x, p.y]).collect();
    let indices = earcutr::earcut(&flat, &[], 2).unwrap_or_default();
    indices
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect()
}

const PALETTE: &[Color] = &[
    Color(70, 130, 180, 200),
    Color(220, 80, 80, 200),
    Color(80, 180, 80, 200),
    Color(200, 160, 50, 200),
    Color(150, 80, 200, 200),
    Color(80, 200, 200, 200),
];

pub struct VisualObserver {
    canvas: Arc<dyn Canvas>,
}

impl VisualObserver {
    pub fn new(canvas: Arc<dyn Canvas>) -> Self {
        Self { canvas }
    }

    fn draw_instance(&self, instance: &framework::Instance, color: &Color, layer: &str) {
        for b in &instance.buildings {
            let triangles = tessellate(&b.vertices);
            self.canvas.draw(DrawCommand::Polygon {
                vertices: b.vertices.clone(),
                triangles,
                fill: Some(*color),
                layer: layer.to_string(),
            });
        }
    }
}

impl framework::PipelineObserver for VisualObserver {
    fn on_event(&self, event: framework::PipelineEvent) {
        match event {
            framework::PipelineEvent::Instance(inst) => {
                self.canvas.draw(DrawCommand::HideAllLayers);
                self.draw_instance(inst, &PALETTE[0], "instance");
            }
            framework::PipelineEvent::Step { index, parts } => {
                self.canvas.draw(DrawCommand::HideAllLayers);
                for (i, part) in parts.iter().enumerate() {
                    let color = &PALETTE[(i + 1) % PALETTE.len()];
                    self.draw_instance(part, color, &format!("step{index}/{i}"));
                }
            }
            framework::PipelineEvent::Solved(_sol) => {}
        }
    }
}
