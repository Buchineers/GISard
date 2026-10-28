use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use framework::{
    AntennaView, Building, EdgeGrid, Evaluator, Instance, IntervalSet, Point, SPATIAL_TOLERANCE,
    bbox, on_segment, parse_antennas, project_onto_segment, ring_edges,
};
use rayon::prelude::*;

use super::format::{Format, ScenarioBlock, decode, encode};
use super::manifest::{Manifest, Scenario};

pub const CLAIM_MARGIN: f64 = 1e-9;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoundaryPolicy {
    #[default]
    Repair,
    Reject,
    Ignore,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub format: Format,
    pub boundary: BoundaryPolicy,
}

#[derive(Debug)]
pub struct Submission {
    pub scenarios: Vec<ScenarioResult>,
    pub format: Format,
}

#[derive(Debug)]
pub struct ScenarioResult {
    pub id: String,
    pub tau: f64,
    pub k: usize,
    pub antennas: Vec<Point>,
    pub serviced: Vec<u32>,
    pub source: PathBuf,
    pub fixes: Fixes,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Fixes {
    pub repaired: usize,
    pub repairs_lossy: usize,
    pub padded: usize,
    pub trimmed: usize,
    pub claims_withheld: usize,
}

impl Fixes {
    pub fn is_clean(&self) -> bool {
        self.repaired == 0 && self.padded == 0 && self.trimmed == 0 && self.claims_withheld == 0
    }
}

#[derive(Debug)]
pub enum Violation {
    MissingSolution {
        scenario: String,
    },
    NonFinite {
        scenario: String,
        index: usize,
        point: Point,
    },
    OffBoundary {
        scenario: String,
        index: usize,
        point: Point,
        distance: f64,
    },
    WrongCount {
        scenario: String,
        got: usize,
        want: usize,
    },
    RoundTrip {
        detail: String,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::MissingSolution { scenario } => {
                write!(f, "{scenario}: no solution found in the campaign directory")
            }
            Violation::NonFinite {
                scenario,
                index,
                point,
            } => write!(
                f,
                "{scenario}: antenna {index} is not finite: ({}, {})",
                point.x, point.y
            ),
            Violation::OffBoundary {
                scenario,
                index,
                point,
                distance,
            } => write!(
                f,
                "{scenario}: antenna {index} at ({}, {}) is not accepted by the boundary policy; nearest edge is {distance:.3e} m away",
                point.x, point.y
            ),
            Violation::WrongCount {
                scenario,
                got,
                want,
            } => write!(f, "{scenario}: {got} antennas, expected exactly {want}"),
            Violation::RoundTrip { detail } => write!(f, "round-trip: {detail}"),
        }
    }
}

pub fn harvest(
    campaign: &Path,
    instance: &Instance,
    options: Options,
) -> Result<Submission, Vec<Violation>> {
    let manifest = Manifest::load(&campaign.join("manifest.json"));
    let evaluator = Evaluator::new(instance);
    let edges = EdgeIndex::new(&instance.buildings);
    let perimeters: Vec<f64> = (0..instance.buildings.len())
        .map(|b| evaluator.perimeter(b))
        .collect();

    let cx = Context {
        instance,
        evaluator: &evaluator,
        edges: &edges,
        perimeters: &perimeters,
        options,
    };

    let mut violations = Vec::new();
    let mut scenarios = Vec::new();
    for scenario in &manifest.scenarios {
        if let Some(result) = harvest_scenario(campaign, scenario, &cx, &mut violations) {
            scenarios.push(result);
        }
    }

    let submission = Submission {
        scenarios,
        format: options.format,
    };
    if let Err(detail) = round_trip(&submission, &cx) {
        violations.push(Violation::RoundTrip { detail });
    }
    if violations.is_empty() {
        Ok(submission)
    } else {
        Err(violations)
    }
}

struct Context<'a> {
    instance: &'a Instance,
    evaluator: &'a Evaluator<'a>,
    edges: &'a EdgeIndex<'a>,
    perimeters: &'a [f64],
    options: Options,
}

fn harvest_scenario(
    campaign: &Path,
    scenario: &Scenario,
    cx: &Context<'_>,
    violations: &mut Vec<Violation>,
) -> Option<ScenarioResult> {
    let sources = solution_candidates(campaign, &scenario.id);
    if sources.is_empty() {
        violations.push(Violation::MissingSolution {
            scenario: scenario.id.clone(),
        });
        return None;
    }

    let mut best: Option<ScenarioResult> = None;
    let mut first_failure = None;
    for source in sources {
        let mut candidate_violations = Vec::new();
        let Some(candidate) = harvest_candidate(source, scenario, cx, &mut candidate_violations)
        else {
            first_failure.get_or_insert(candidate_violations);
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|incumbent| candidate.serviced.len() > incumbent.serviced.len())
        {
            best = Some(candidate);
        }
    }
    if best.is_none() {
        violations.extend(first_failure.unwrap_or_else(|| {
            vec![Violation::MissingSolution {
                scenario: scenario.id.clone(),
            }]
        }));
    }
    best
}

fn harvest_candidate(
    source: PathBuf,
    scenario: &Scenario,
    cx: &Context<'_>,
    violations: &mut Vec<Violation>,
) -> Option<ScenarioResult> {
    let antennas = parse_antennas(source.to_str().expect("non-utf8 solution path"));

    for (i, &p) in antennas.iter().enumerate() {
        if !p.x.is_finite() || !p.y.is_finite() {
            violations.push(Violation::NonFinite {
                scenario: scenario.id.clone(),
                index: i,
                point: p,
            });
            return None;
        }
    }

    let mut placement = Placement::new(cx.evaluator, cx.instance.buildings.len(), antennas);
    let mut fixes = Fixes::default();

    repair_boundary(&mut placement, scenario, cx, &mut fixes, violations);
    if violations
        .iter()
        .any(|v| matches!(v, Violation::OffBoundary { .. }))
    {
        return None;
    }

    match placement.len().cmp(&scenario.k) {
        std::cmp::Ordering::Less => {
            fixes.padded = pad(&mut placement, cx.edges, scenario.k);
        }
        std::cmp::Ordering::Greater => {
            fixes.trimmed = trim(&mut placement, scenario, cx.perimeters);
        }
        std::cmp::Ordering::Equal => {}
    }
    if placement.len() != scenario.k {
        violations.push(Violation::WrongCount {
            scenario: scenario.id.clone(),
            got: placement.len(),
            want: scenario.k,
        });
        return None;
    }

    let covered = placement.covered();
    let claimed = claims(&covered, cx.perimeters, scenario.tau);
    fixes.claims_withheld = serviced_exactly(&covered, cx.perimeters, scenario.tau) - claimed.len();
    let serviced: Vec<u32> = claimed
        .iter()
        .map(|&b| cx.instance.buildings[b].id)
        .collect();

    Some(ScenarioResult {
        id: scenario.id.clone(),
        tau: scenario.tau,
        k: scenario.k,
        antennas: placement.antennas,
        serviced,
        source,
        fixes,
    })
}

fn round_trip(submission: &Submission, cx: &Context<'_>) -> Result<(), String> {
    let text = encode(&submission.blocks(), submission.format);
    let blocks = decode(&text)?;
    if blocks.len() != submission.scenarios.len() {
        return Err(format!(
            "{} blocks decoded, {} written",
            blocks.len(),
            submission.scenarios.len()
        ));
    }
    for (block, result) in blocks.iter().zip(&submission.scenarios) {
        if block.tau.to_bits() != result.tau.to_bits() || block.k != result.k {
            return Err(format!("{}: (tau, k) changed under round-trip", result.id));
        }
        if block.antennas.len() != result.antennas.len() {
            return Err(format!("{}: antenna count changed", result.id));
        }
        for (i, (a, b)) in block.antennas.iter().zip(&result.antennas).enumerate() {
            if a.x.to_bits() != b.x.to_bits() || a.y.to_bits() != b.y.to_bits() {
                return Err(format!("{}: antenna {i} lost precision", result.id));
            }
        }
        if block.serviced != result.serviced {
            return Err(format!("{}: claimed ids changed", result.id));
        }

        let placement = Placement::new(
            cx.evaluator,
            cx.instance.buildings.len(),
            block.antennas.clone(),
        );
        let covered = placement.covered();
        let verified: Vec<u32> = claims(&covered, cx.perimeters, block.tau)
            .iter()
            .map(|&b| cx.instance.buildings[b].id)
            .collect();
        if verified != block.serviced {
            let unverified: Vec<u32> = block
                .serviced
                .iter()
                .filter(|id| !verified.contains(id))
                .take(8)
                .copied()
                .collect();
            return Err(format!(
                "{}: rescoring the emitted file claims {} buildings, we wrote {} (unverified: {unverified:?})",
                result.id,
                verified.len(),
                block.serviced.len(),
            ));
        }
    }
    Ok(())
}

impl Submission {
    pub fn blocks(&self) -> Vec<ScenarioBlock> {
        self.scenarios
            .iter()
            .map(|s| ScenarioBlock {
                tau: s.tau,
                k: s.k,
                antennas: s.antennas.clone(),
                serviced: s.serviced.clone(),
            })
            .collect()
    }

    pub fn write(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).expect("failed to create submission directory");
        }
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, encode(&self.blocks(), self.format))
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", temporary.display()));
        fs::rename(&temporary, path)
            .unwrap_or_else(|e| panic!("failed to publish {}: {e}", path.display()));
    }
}

fn solution_candidates(campaign: &Path, scenario: &str) -> Vec<PathBuf> {
    let dir = campaign.join("best").join(scenario);
    let mut candidates: Vec<(u64, PathBuf)> = fs::read_dir(dir.join("cand"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_stem()?.to_str()?;
            let score: u64 = name.split('-').next()?.parse().ok()?;
            fs::read_to_string(&path)
                .is_ok_and(|body| !body.trim().is_empty())
                .then_some((score, path))
        })
        .collect();
    candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut paths: Vec<_> = candidates.into_iter().map(|(_, path)| path).collect();
    let fallback = dir.join("best.txt");
    if fallback.is_file() {
        paths.push(fallback);
    }
    paths
}

pub(super) fn best_solution(campaign: &Path, scenario: &str) -> Option<PathBuf> {
    solution_candidates(campaign, scenario).into_iter().next()
}

struct Placement<'a> {
    evaluator: &'a Evaluator<'a>,
    antennas: Vec<Point>,
    views: Vec<AntennaView>,
    buildings: usize,
}

impl<'a> Placement<'a> {
    fn new(evaluator: &'a Evaluator<'a>, buildings: usize, antennas: Vec<Point>) -> Self {
        let views = antennas
            .par_iter()
            .map(|&p| evaluator.antenna_view(p))
            .collect();
        Placement {
            evaluator,
            antennas,
            views,
            buildings,
        }
    }

    fn len(&self) -> usize {
        self.antennas.len()
    }

    fn covered_with(&self, skip: Option<usize>, extra: Option<&AntennaView>) -> Vec<f64> {
        let mut sets = vec![IntervalSet::new(); self.buildings];
        for (i, view) in self.views.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            for (b, set) in &view.covered {
                sets[*b as usize].union_with(set);
            }
        }
        if let Some(view) = extra {
            for (b, set) in &view.covered {
                sets[*b as usize].union_with(set);
            }
        }
        sets.iter().map(IntervalSet::measure).collect()
    }

    fn covered(&self) -> Vec<f64> {
        self.covered_with(None, None)
    }

    fn set(&mut self, i: usize, p: Point, view: AntennaView) {
        self.antennas[i] = p;
        self.views[i] = view;
    }

    fn push(&mut self, p: Point) {
        self.views.push(self.evaluator.antenna_view(p));
        self.antennas.push(p);
    }

    fn remove(&mut self, i: usize) {
        self.antennas.remove(i);
        self.views.remove(i);
    }
}

fn claims(covered: &[f64], perimeters: &[f64], tau: f64) -> Vec<usize> {
    (0..covered.len())
        .filter(|&b| {
            perimeters[b] > 0.0 && covered[b] >= tau * perimeters[b] * (1.0 + CLAIM_MARGIN)
        })
        .collect()
}

fn serviced_exactly(covered: &[f64], perimeters: &[f64], tau: f64) -> usize {
    (0..covered.len())
        .filter(|&b| perimeters[b] > 0.0 && covered[b] >= tau * perimeters[b])
        .count()
}

fn repair_boundary(
    placement: &mut Placement<'_>,
    scenario: &Scenario,
    cx: &Context<'_>,
    fixes: &mut Fixes,
    violations: &mut Vec<Violation>,
) {
    let (edges, perimeters, policy) = (cx.edges, cx.perimeters, cx.options.boundary);
    if policy == BoundaryPolicy::Ignore {
        return;
    }
    let failures: Vec<usize> = (0..placement.len())
        .into_par_iter()
        .filter(|&i| !edges.on_boundary(placement.antennas[i]))
        .collect();

    let baseline = (policy == BoundaryPolicy::Repair && !failures.is_empty())
        .then(|| claims(&placement.covered(), perimeters, scenario.tau).len());
    let mut repairs = Vec::new();

    for i in failures {
        let p = placement.antennas[i];
        if policy == BoundaryPolicy::Reject {
            let distance = edges
                .nearest_edge(p)
                .map_or(f64::INFINITY, |(.., distance)| distance);
            violations.push(Violation::OffBoundary {
                scenario: scenario.id.clone(),
                index: i,
                point: p,
                distance,
            });
            continue;
        }
        let Some((_, point, _)) = placement.evaluator.snap(p, SPATIAL_TOLERANCE) else {
            let distance = edges
                .nearest_edge(p)
                .map_or(f64::INFINITY, |(.., distance)| distance);
            violations.push(Violation::OffBoundary {
                scenario: scenario.id.clone(),
                index: i,
                point: p,
                distance,
            });
            continue;
        };
        debug_assert!(
            edges.on_boundary(point),
            "an officially snapped antenna must pass the exact test"
        );
        repairs.push((i, point));
    }

    let replacements: Vec<_> = repairs
        .par_iter()
        .map(|&(i, point)| (i, point, placement.evaluator.antenna_view(point)))
        .collect();
    fixes.repaired += replacements.len();
    for (i, point, view) in replacements {
        placement.set(i, point, view);
    }
    if let Some(baseline) = baseline {
        let repaired = claims(&placement.covered(), perimeters, scenario.tau).len();
        fixes.repairs_lossy += baseline.saturating_sub(repaired);
    }
}

fn pad(placement: &mut Placement<'_>, edges: &EdgeIndex<'_>, k: usize) -> usize {
    let mut added = 0;
    while placement.len() < k {
        let Some(v) = edges.vertex(placement.len()) else {
            break;
        };
        placement.push(v);
        added += 1;
    }
    added
}

fn trim(placement: &mut Placement<'_>, scenario: &Scenario, perimeters: &[f64]) -> usize {
    let mut dropped = 0;
    while placement.len() > scenario.k {
        let worst = (0..placement.len())
            .into_par_iter()
            .map(|i| {
                let covered = placement.covered_with(Some(i), None);
                let score = claims(&covered, perimeters, scenario.tau).len();
                let total: f64 = covered.iter().sum();
                (score, total, i)
            })
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)))
            .expect("a non-empty placement");
        placement.remove(worst.2);
        dropped += 1;
    }
    dropped
}

struct EdgeIndex<'a> {
    buildings: &'a [Building],
    grid: EdgeGrid,
    span: f64,
}

impl<'a> EdgeIndex<'a> {
    fn new(buildings: &'a [Building]) -> Self {
        let (min, max) = bbox(buildings);
        let span = if min.x.is_finite() && max.x.is_finite() {
            (max - min).norm()
        } else {
            0.0
        };
        EdgeIndex {
            buildings,
            grid: EdgeGrid::with_auto_cell_size(buildings),
            span,
        }
    }

    fn on_boundary(&self, p: Point) -> bool {
        self.near(p, 0.0).any(|(a, b)| on_segment(a, b, p))
    }

    fn nearest_edge(&self, p: Point) -> Option<(Point, Point, f64)> {
        let mut radius = (self.span / 1024.0).max(f64::MIN_POSITIVE);
        loop {
            let best = self
                .near(p, radius)
                .map(|(a, b)| (a, b, (project_onto_segment(p, a, b) - p).norm()))
                .min_by(|x, y| x.2.total_cmp(&y.2));
            if best.is_some() || radius > self.span {
                return best;
            }
            radius *= 2.0;
        }
    }

    fn near(&self, p: Point, radius: f64) -> impl Iterator<Item = (Point, Point)> + '_ {
        self.grid
            .buildings_within(p, radius)
            .into_iter()
            .flat_map(move |bi| ring_edges(&self.buildings[bi].vertices))
    }

    fn vertex(&self, n: usize) -> Option<Point> {
        let vertices: Vec<Point> = self
            .buildings
            .iter()
            .filter_map(|b| b.vertices.first().copied())
            .collect();
        vertices.get(n % vertices.len().max(1)).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use framework::test_utils::{instance, p, square};

    fn campaign_dir(scenarios: &[Scenario], solutions: &[(&str, &str)]) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gisard-harvest-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Manifest {
            dataset: PathBuf::from("unused.geojson"),
            scenarios: scenarios.to_vec(),
            start_unix: 0,
            deadline_unix: 0,
            reserve_seconds: 0,
            duration_seconds: 0,
            spacings: vec![],
            buildings: 0,
        }
        .save(&dir.join("manifest.json"));
        for (id, antennas) in solutions {
            let cand = dir.join("best").join(id).join("cand");
            fs::create_dir_all(&cand).unwrap();
            fs::write(cand.join("0000000042-abcdef.txt"), antennas).unwrap();
        }
        dir
    }

    fn write_candidate(dir: &Path, scenario: &str, name: &str, antennas: &str) -> PathBuf {
        let cand = dir.join("best").join(scenario).join("cand");
        fs::create_dir_all(&cand).unwrap();
        let path = cand.join(name);
        fs::write(&path, antennas).unwrap();
        path
    }

    #[test]
    fn claims_respect_the_margin() {
        let perimeters = [4.0];
        let covered = [2.0];
        assert_eq!(serviced_exactly(&covered, &perimeters, 0.5), 1);
        assert!(claims(&covered, &perimeters, 0.5).is_empty());
        assert_eq!(claims(&[2.001], &perimeters, 0.5), vec![0]);
    }

    #[test]
    fn degenerate_buildings_are_never_claimed() {
        assert!(claims(&[0.0], &[0.0], 0.5).is_empty());
        assert_eq!(serviced_exactly(&[0.0], &[0.0], 0.5), 0);
    }

    #[test]
    fn vertices_pass_the_exact_test_and_drifted_points_do_not() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let edges = EdgeIndex::new(&inst.buildings);
        assert!(edges.on_boundary(p(0.0, 0.0)));
        assert!(edges.on_boundary(p(5.0, 0.0)));
        assert!(!edges.on_boundary(p(5.0, 1e-12)));
    }

    #[test]
    fn nearest_edge_returns_the_endpoints_of_the_real_edge() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let edges = EdgeIndex::new(&inst.buildings);
        let (a, b, d) = edges.nearest_edge(p(5.0, -1.0)).expect("an edge");
        assert!((d - 1.0).abs() < 1e-12, "distance {d}");
        assert!(a.y == 0.0 && b.y == 0.0, "expected the bottom edge");
    }

    #[test]
    fn repair_snaps_a_drifted_point_in_place() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let evaluator = Evaluator::new(&inst);
        let edges = EdgeIndex::new(&inst.buildings);
        let perimeters = vec![evaluator.perimeter(0)];
        let scenario = Scenario::new(0.5, 1);

        let mut placement = Placement::new(&evaluator, 1, vec![p(5.0, 1e-12)]);
        let mut fixes = Fixes::default();
        let mut violations = Vec::new();
        let cx = Context {
            instance: &inst,
            evaluator: &evaluator,
            edges: &edges,
            perimeters: &perimeters,
            options: Options::default(),
        };
        repair_boundary(&mut placement, &scenario, &cx, &mut fixes, &mut violations);

        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(fixes.repaired, 1);
        assert_eq!(placement.antennas[0], p(5.0, 0.0));
        assert!(edges.on_boundary(placement.antennas[0]));
    }

    #[test]
    fn repair_rejects_a_point_beyond_the_official_tolerance() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let evaluator = Evaluator::new(&inst);
        let edges = EdgeIndex::new(&inst.buildings);
        let perimeters = vec![evaluator.perimeter(0)];
        let scenario = Scenario::new(0.5, 1);
        let mut placement = Placement::new(&evaluator, 1, vec![p(5.0, 0.001_001)]);
        let mut fixes = Fixes::default();
        let mut violations = Vec::new();
        let cx = Context {
            instance: &inst,
            evaluator: &evaluator,
            edges: &edges,
            perimeters: &perimeters,
            options: Options::default(),
        };

        repair_boundary(&mut placement, &scenario, &cx, &mut fixes, &mut violations);

        assert_eq!(fixes.repaired, 0);
        assert!(matches!(
            violations.as_slice(),
            [Violation::OffBoundary { .. }]
        ));
    }

    #[test]
    fn harvests_repairs_and_claims_end_to_end() {
        let inst = instance(vec![square(7, 0.0, 0.0)], 0.25);
        let scenarios = vec![Scenario::new(0.25, 2)];
        let dir = campaign_dir(&scenarios, &[(&scenarios[0].id, "0,0\n5.0,1e-12\n")]);

        let submission = harvest(&dir, &inst, Options::default()).expect("gates should pass");
        let result = &submission.scenarios[0];
        assert_eq!(result.antennas.len(), 2);
        assert_eq!(result.fixes.repaired, 1);
        assert_eq!(result.serviced, vec![7]);

        let edges = EdgeIndex::new(&inst.buildings);
        assert!(result.antennas.iter().all(|&a| edges.on_boundary(a)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_chooses_the_best_strictly_rescored_candidate() {
        let inst = instance(vec![square(7, 0.0, 0.0), square(8, 20.0, 0.0)], 0.3);
        let scenarios = vec![Scenario::new(0.3, 1)];
        let dir = campaign_dir(&scenarios, &[]);
        write_candidate(&dir, &scenarios[0].id, "0000000099-high.txt", "0,0\n");
        let better = write_candidate(&dir, &scenarios[0].id, "0000000001-lower.txt", "10,0\n");

        let submission = harvest(&dir, &inst, Options::default()).expect("both candidates valid");
        let result = &submission.scenarios[0];
        assert_eq!(result.source, better);
        assert_eq!(result.serviced, vec![7, 8]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_solution_is_a_violation() {
        let inst = instance(vec![square(7, 0.0, 0.0)], 0.5);
        let scenarios = vec![Scenario::new(0.5, 2)];
        let dir = campaign_dir(&scenarios, &[]);

        let violations = harvest(&dir, &inst, Options::default()).expect_err("no solution exists");
        assert!(matches!(
            violations.as_slice(),
            [Violation::MissingSolution { .. }]
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_short_solution_is_padded_to_k() {
        let inst = instance(vec![square(7, 0.0, 0.0)], 0.5);
        let scenarios = vec![Scenario::new(0.5, 3)];
        let dir = campaign_dir(&scenarios, &[(&scenarios[0].id, "0,0\n")]);

        let submission = harvest(&dir, &inst, Options::default()).expect("padding should recover");
        assert_eq!(submission.scenarios[0].antennas.len(), 3);
        assert_eq!(submission.scenarios[0].fixes.padded, 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_over_long_solution_is_trimmed_to_k() {
        let inst = instance(vec![square(7, 0.0, 0.0)], 0.5);
        let scenarios = vec![Scenario::new(0.5, 1)];
        let dir = campaign_dir(&scenarios, &[(&scenarios[0].id, "0,0\n10,0\n10,10\n")]);

        let submission = harvest(&dir, &inst, Options::default()).expect("trimming should recover");
        assert_eq!(submission.scenarios[0].antennas.len(), 1);
        assert_eq!(submission.scenarios[0].fixes.trimmed, 2);
        fs::remove_dir_all(&dir).ok();
    }
}
