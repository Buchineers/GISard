use crate::antennas::ANTENNA_SNAP_DIST;
use framework::{AntennaView, Evaluator, Instance, IntervalSet, Point, Solver};
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use visualizer::{Canvas, Color, DrawCommand, UiEvent, coverage_color, tessellate};

const SNAP_DIST: f64 = ANTENNA_SNAP_DIST;
const ANTENNA_PICK: f64 = 10.0;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Place,
    Inspect,
}

const CONTRIBUTOR_COLORS: [Color; 6] = [
    Color(116, 216, 255, 255),
    Color(255, 209, 102, 255),
    Color(255, 112, 166, 255),
    Color(142, 255, 173, 255),
    Color(186, 146, 255, 255),
    Color(255, 157, 92, 255),
];
const L_BUILDINGS: &str = "buildings";
const L_COVERAGE_FILL: &str = "coverage heat";
const L_SERVICED: &str = "serviced";
const L_COVERED: &str = "covered perimeter";
const L_REGIONS: &str = "antenna regions";
const L_PREVIEW_REGION: &str = "preview region";
const L_PREVIEW: &str = "preview";
const L_ANTENNAS: &str = "antennas";

pub struct Inspector<'a> {
    instance: &'a Instance,
    buildings: &'a [framework::Building],
    canvas: Arc<dyn Canvas>,
    tau: f64,
    evaluator: Evaluator<'a>,
    needs: Vec<f64>,
    acc: Vec<IntervalSet>,
    contribs: Vec<Vec<(u32, f64)>>,
    serviced: Vec<bool>,
    antennas: Vec<Point>,
    mode: Mode,
    solver: Box<dyn Solver>,
}

struct Placement {
    on: Point,
    view: AntennaView,
    gain: usize,
    lit: f64,
}

impl<'a> Inspector<'a> {
    pub fn new(instance: &'a Instance, canvas: Arc<dyn Canvas>, solver: Box<dyn Solver>) -> Self {
        let buildings = &instance.buildings;
        let tau = instance.tau;
        canvas.draw(DrawCommand::HideAllLayers);
        for layer in [
            L_BUILDINGS,
            L_COVERAGE_FILL,
            L_SERVICED,
            L_COVERED,
            L_REGIONS,
            L_PREVIEW_REGION,
            L_PREVIEW,
            L_ANTENNAS,
        ] {
            canvas.draw(DrawCommand::ClearLayer {
                layer: layer.into(),
            });
        }
        for b in buildings {
            canvas.draw(DrawCommand::Polygon {
                vertices: b.vertices.clone(),
                triangles: tessellate(&b.vertices),
                fill: Some(Color::GRAY),
                layer: L_BUILDINGS.into(),
            });
        }
        canvas.draw(DrawCommand::Status {
            text: "building mesh...".into(),
        });
        let evaluator = Evaluator::new(instance);
        let needs = (0..buildings.len())
            .map(|b| tau * evaluator.perimeter(b))
            .collect();
        canvas.draw(DrawCommand::SetControls {
            fields: Vec::new(),
            button: String::new(),
        });
        canvas.draw(DrawCommand::SetModes {
            options: vec!["place".into(), "inspect".into()],
            selected: 0,
        });
        let state = Self {
            instance,
            buildings,
            canvas,
            tau,
            evaluator,
            needs,
            acc: vec![IntervalSet::new(); buildings.len()],
            contribs: vec![Vec::new(); buildings.len()],
            serviced: vec![false; buildings.len()],
            antennas: Vec::new(),
            mode: Mode::Place,
            solver,
        };
        state.update_status(None, None);
        state
    }

    pub fn start_in_inspect(mut self) -> Self {
        self.set_mode(Mode::Inspect, true);
        self.update_status(None, None);
        self
    }

    fn reset(&mut self, tau: f64) {
        self.tau = tau;
        self.antennas.clear();
        for b in 0..self.buildings.len() {
            self.acc[b] = IntervalSet::new();
            self.needs[b] = tau * self.evaluator.perimeter(b);
            self.contribs[b].clear();
            self.serviced[b] = false;
        }
        for layer in [
            L_SERVICED,
            L_COVERAGE_FILL,
            L_COVERED,
            L_REGIONS,
            L_PREVIEW_REGION,
            L_PREVIEW,
            L_ANTENNAS,
        ] {
            self.canvas.draw(DrawCommand::ClearLayer {
                layer: layer.into(),
            });
        }
    }

    fn run_solver(&mut self, values: &[f64]) {
        let k = (values.first().copied().unwrap_or(50.0) as usize).max(1);
        let tau = values.get(1).copied().unwrap_or(self.tau).clamp(0.01, 1.0);
        self.reset(tau);
        self.canvas.draw(DrawCommand::Status {
            text: format!("solving k={k}, tau={tau}..."),
        });
        let mut inst = self.instance.clone();
        inst.k = k;
        inst.tau = tau;
        let solution = self.solver.solve(inst);
        self.preload(&solution.antennas);
    }

    pub fn preload(&mut self, antennas: &[Point]) {
        self.reset(self.tau);
        self.canvas.draw(DrawCommand::SetLayerVisible {
            layer: L_REGIONS.into(),
            visible: antennas.len() <= 50,
        });
        let mut missed = 0;
        for &a in antennas {
            if !self.place(a, false) {
                missed += 1;
            }
        }
        self.draw_coverage_heat();
        self.set_mode(Mode::Inspect, true);
        self.update_status(None, None);
        eprintln!(
            "loaded {} antennas ({missed} failed to snap), {} serviced at tau={}",
            antennas.len() - missed,
            self.serviced.iter().filter(|&&s| s).count(),
            self.tau
        );
    }

    pub fn run(mut self, events: visualizer::Receiver<UiEvent>) {
        while let Ok(first) = events.recv() {
            let mut hover = None;
            let mut pending = vec![first];
            while let Ok(more) = events.try_recv() {
                pending.push(more);
            }
            for event in pending {
                match event {
                    UiEvent::Click(p) => {
                        match self.mode {
                            Mode::Place => {
                                self.place(p, true);
                            }
                            Mode::Inspect => self.inspect(p),
                        }
                        hover = None;
                    }
                    UiEvent::Hover(p) => hover = Some(p),
                    UiEvent::Action(values) => {
                        self.run_solver(&values);
                        hover = None;
                    }
                    UiEvent::Mode(i) => {
                        self.set_mode(if i == 1 { Mode::Inspect } else { Mode::Place }, false);
                        hover = None;
                    }
                }
            }
            if let Some(p) = hover {
                self.preview(p);
            }
        }
    }

    pub fn run_with_solution_receiver(
        mut self,
        events: visualizer::Receiver<UiEvent>,
        solutions: mpsc::Receiver<Vec<Point>>,
    ) {
        loop {
            while let Ok(antennas) = solutions.try_recv() {
                self.preload(&antennas);
            }
            let first = match events.recv_timeout(Duration::from_millis(30)) {
                Ok(event) => event,
                Err(visualizer::RecvTimeoutError::Timeout) => continue,
                Err(visualizer::RecvTimeoutError::Disconnected) => break,
            };
            self.handle_events(first, &events);
        }
    }

    fn handle_events(&mut self, first: UiEvent, events: &visualizer::Receiver<UiEvent>) {
        let mut hover = None;
        let mut pending = vec![first];
        while let Ok(more) = events.try_recv() {
            pending.push(more);
        }
        for event in pending {
            match event {
                UiEvent::Click(p) => {
                    match self.mode {
                        Mode::Place => {
                            self.place(p, true);
                        }
                        Mode::Inspect => self.inspect(p),
                    }
                    hover = None;
                }
                UiEvent::Hover(p) => hover = Some(p),
                UiEvent::Action(values) => {
                    self.run_solver(&values);
                    hover = None;
                }
                UiEvent::Mode(i) => {
                    self.set_mode(if i == 1 { Mode::Inspect } else { Mode::Place }, false);
                    hover = None;
                }
            }
        }
        if let Some(p) = hover {
            self.preview(p);
        }
    }

    fn query(&self, p: Point) -> Option<Placement> {
        let (_, on, probe) = self.evaluator.snap(p, SNAP_DIST)?;
        let view = self.evaluator.antenna_view(probe);
        let mut gain = 0;
        let mut lit = 0.0;
        for (b, set) in &view.covered {
            let b = *b as usize;
            lit += set.measure();
            if !self.serviced[b]
                && self.needs[b] > 0.0
                && self.acc[b].union_measure(set) >= self.needs[b]
            {
                gain += 1;
            }
        }
        Some(Placement {
            on,
            view,
            gain,
            lit,
        })
    }

    fn set_mode(&mut self, mode: Mode, announce: bool) {
        self.mode = mode;
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_PREVIEW.into(),
        });
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_PREVIEW_REGION.into(),
        });
        if announce {
            self.canvas.draw(DrawCommand::SetModes {
                options: vec!["place".into(), "inspect".into()],
                selected: if mode == Mode::Inspect { 1 } else { 0 },
            });
        }
    }

    fn inspect(&self, p: Point) {
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_PREVIEW.into(),
        });
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_PREVIEW_REGION.into(),
        });
        let nearest = self
            .antennas
            .iter()
            .enumerate()
            .map(|(i, a)| (i, (*a - p).norm_sq()))
            .min_by(|(_, d1), (_, d2)| d1.total_cmp(d2));
        if let Some((ai, d2)) = nearest
            && d2 <= ANTENNA_PICK * ANTENNA_PICK
        {
            self.inspect_antenna(ai);
            return;
        }
        let Some(b) = self.evaluator.building_at(p) else {
            for layer in [L_REGIONS, "ga regions"] {
                self.canvas.draw(DrawCommand::SetLayerVisible {
                    layer: layer.into(),
                    visible: true,
                });
            }
            self.update_status(None, None);
            return;
        };
        for layer in [L_REGIONS, "ga regions"] {
            self.canvas.draw(DrawCommand::SetLayerVisible {
                layer: layer.into(),
                visible: false,
            });
        }
        let mut ranked = self.contribs[b].clone();
        ranked.sort_unstable_by(|(_, l1), (_, l2)| l2.total_cmp(l1));
        for (a, b) in framework::ring_edges(&self.buildings[b].vertices) {
            self.canvas.draw(DrawCommand::Line {
                from: a,
                to: b,
                color: Color::WHITE,
                layer: L_PREVIEW.into(),
            });
        }
        for (rank, &(ai, _)) in ranked.iter().take(CONTRIBUTOR_COLORS.len()).enumerate() {
            let color = CONTRIBUTOR_COLORS[rank];
            self.draw_antenna_region(ai as usize, Color(color.0, color.1, color.2, 38));
            self.canvas.draw(DrawCommand::Points {
                positions: vec![self.antennas[ai as usize]],
                radius: 8.0,
                color,
                layer: L_PREVIEW.into(),
            });
        }
        self.update_status(Some(b), None);
    }

    fn inspect_antenna(&self, ai: usize) {
        for layer in [L_REGIONS, "ga regions"] {
            self.canvas.draw(DrawCommand::SetLayerVisible {
                layer: layer.into(),
                visible: false,
            });
        }
        let a = self.antennas[ai];
        let Some((_, on, probe)) = self.evaluator.snap(a, SNAP_DIST) else {
            return;
        };
        let view = self.evaluator.antenna_view(probe);
        self.draw_antenna_region(ai, Color::PREVIEW_FILL);
        for w in &view.region.walls {
            self.canvas.draw(DrawCommand::Line {
                from: w.a,
                to: w.b,
                color: Color::YELLOW,
                layer: L_PREVIEW.into(),
            });
        }
        self.canvas.draw(DrawCommand::Points {
            positions: vec![on],
            radius: 8.0,
            color: Color::WHITE,
            layer: L_PREVIEW.into(),
        });

        let mut text = self.status_header();
        let lit: f64 = view.covered.iter().map(|(_, set)| set.measure()).sum();
        let _ = write!(
            text,
            "\nantenna {} at ({:.1}, {:.1})\nlights {lit:.0} perimeter on {} buildings\n",
            ai + 1,
            on.x,
            on.y,
            view.covered.len()
        );
        let mut ranked: Vec<(usize, f64)> = view
            .covered
            .iter()
            .map(|(b, set)| (*b as usize, set.measure()))
            .collect();
        ranked.sort_unstable_by(|(_, l1), (_, l2)| l2.total_cmp(l1));
        for (b, len) in ranked.iter().take(8) {
            let pct = 100.0 * len / self.evaluator.perimeter(*b).max(f64::MIN_POSITIVE);
            let _ = writeln!(
                text,
                "  building {}: {pct:.1}%{}",
                self.buildings[*b].id,
                if self.serviced[*b] { " (serviced)" } else { "" }
            );
        }
        self.canvas.draw(DrawCommand::Status { text });
    }

    fn draw_antenna_region(&self, ai: usize, fill: Color) {
        let a = self.antennas[ai];
        let Some((_, on, probe)) = self.evaluator.snap(a, SNAP_DIST) else {
            return;
        };
        let view = self.evaluator.antenna_view(probe);
        let (vertices, triangles) = view.region.fan(on);
        self.canvas.draw(DrawCommand::Polygon {
            vertices,
            triangles,
            fill: Some(fill),
            layer: L_PREVIEW_REGION.into(),
        });
    }

    fn preview(&self, p: Point) {
        let hovered = self.evaluator.building_at(p);
        if self.mode == Mode::Inspect {
            self.update_status(hovered, None);
            return;
        }
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_PREVIEW.into(),
        });
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_PREVIEW_REGION.into(),
        });
        let placement = self.query(p);
        self.update_status(hovered, placement.as_ref());
        let Some(Placement { on, view, gain, .. }) = placement else {
            return;
        };
        let (vertices, triangles) = view.region.fan(on);
        self.canvas.draw(DrawCommand::Polygon {
            vertices,
            triangles,
            fill: Some(Color::PREVIEW_FILL),
            layer: L_PREVIEW_REGION.into(),
        });
        for w in &view.region.walls {
            self.canvas.draw(DrawCommand::Line {
                from: w.a,
                to: w.b,
                color: Color::YELLOW,
                layer: L_PREVIEW.into(),
            });
        }
        self.canvas.draw(DrawCommand::Points {
            positions: vec![on],
            radius: 5.0,
            color: Color::WHITE,
            layer: L_PREVIEW.into(),
        });
        self.canvas.draw(DrawCommand::Text {
            pos: on + Point::new(0.0, 8.0),
            text: format!("+{gain}"),
            color: Color::WHITE,
            layer: L_PREVIEW.into(),
        });
    }

    fn place(&mut self, p: Point, verbose: bool) -> bool {
        if verbose {
            self.canvas.draw(DrawCommand::ClearLayer {
                layer: L_PREVIEW.into(),
            });
            self.canvas.draw(DrawCommand::ClearLayer {
                layer: L_PREVIEW_REGION.into(),
            });
        }
        let Some(Placement { on, view, gain, .. }) = self.query(p) else {
            return false;
        };
        let antenna_index = self.antennas.len() as u32;
        self.antennas.push(on);
        for (b, set) in &view.covered {
            let b = *b as usize;
            self.contribs[b].push((antenna_index, set.measure()));
            self.acc[b].union_with(set);
            self.draw_covered(b);
            if !self.serviced[b] && self.needs[b] > 0.0 && self.acc[b].measure() >= self.needs[b] {
                self.serviced[b] = true;
                let ring = &self.buildings[b].vertices;
                self.canvas.draw(DrawCommand::Polygon {
                    vertices: ring.clone(),
                    triangles: tessellate(ring),
                    fill: Some(Color::SERVICED),
                    layer: L_SERVICED.into(),
                });
            }
        }
        let (vertices, triangles) = view.region.fan(on);
        self.canvas.draw(DrawCommand::Polygon {
            vertices,
            triangles,
            fill: Some(Color::REGION_FILL),
            layer: L_REGIONS.into(),
        });
        self.canvas.draw(DrawCommand::Points {
            positions: vec![on],
            radius: 5.0,
            color: Color::YELLOW,
            layer: L_ANTENNAS.into(),
        });
        self.canvas.draw(DrawCommand::Text {
            pos: on + Point::new(0.0, 8.0),
            text: format!("{}", antenna_index + 1),
            color: Color::YELLOW,
            layer: L_ANTENNAS.into(),
        });
        if verbose {
            self.draw_coverage_heat();
            self.update_status(None, None);
            eprintln!(
                "antenna {} at ({:.3}, {:.3}): +{gain} -> {} serviced at tau={}",
                antenna_index + 1,
                on.x,
                on.y,
                self.serviced.iter().filter(|&&s| s).count(),
                self.tau
            );
        }
        true
    }

    fn draw_coverage_heat(&self) {
        self.canvas.draw(DrawCommand::ClearLayer {
            layer: L_COVERAGE_FILL.into(),
        });
        for b in 0..self.buildings.len() {
            let need = self.needs[b].max(f64::MIN_POSITIVE);
            let progress = (self.acc[b].measure() / need).clamp(0.0, 1.0);
            if progress <= 0.0 {
                continue;
            }
            let ring = &self.buildings[b].vertices;
            self.canvas.draw(DrawCommand::Polygon {
                vertices: ring.clone(),
                triangles: tessellate(ring),
                fill: Some(coverage_color(progress)),
                layer: L_COVERAGE_FILL.into(),
            });
        }
    }

    fn draw_covered(&self, b: usize) {
        for (from, to) in self.evaluator.covered_segments(b, &self.acc[b]) {
            self.canvas.draw(DrawCommand::Line {
                from,
                to,
                color: Color::COVERED,
                layer: L_COVERED.into(),
            });
        }
    }

    fn status_header(&self) -> String {
        format!(
            "{} antennas | {} / {} serviced\ntau {} \n",
            self.antennas.len(),
            self.serviced.iter().filter(|&&s| s).count(),
            self.buildings.len(),
            self.tau
        )
    }

    fn update_status(&self, hovered: Option<usize>, placement: Option<&Placement>) {
        let mut text = self.status_header();
        if let Some(b) = hovered {
            let covered = self.acc[b].measure();
            let total = self.evaluator.perimeter(b);
            let ratio = if total > 0.0 {
                100.0 * covered / total
            } else {
                0.0
            };
            let _ = write!(
                text,
                "\nbuilding {}\n{ratio:.1}% covered ({covered:.0}/{total:.0}, need {:.0})\n{}\n",
                self.buildings[b].id,
                self.needs[b],
                if self.serviced[b] {
                    "SERVICED"
                } else {
                    "not serviced"
                },
            );
            let mut ranked = self.contribs[b].clone();
            ranked.sort_unstable_by(|(_, l1), (_, l2)| l2.total_cmp(l1));
            for (ai, len) in ranked.iter().take(5) {
                let pct = 100.0 * len / total.max(f64::MIN_POSITIVE);
                let _ = writeln!(text, "  antenna {}: {pct:.1}%", ai + 1);
            }
        }
        if let Some(pl) = placement {
            let _ = write!(
                text,
                "\nplace here: +{} serviced\nlights {:.0} perimeter on {} buildings",
                pl.gain,
                pl.lit,
                pl.view.covered.len()
            );
        }
        self.canvas.draw(DrawCommand::Status { text });
    }
}
