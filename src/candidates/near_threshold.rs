use framework::{CoverageGoals, IntervalSet};
use rayon::prelude::*;

use super::CandidateContext;
use crate::antennas::ANTENNA_SNAP_DIST;

pub(crate) struct NearThreshold {
    pub goals: CoverageGoals,
    pub acc: Vec<IntervalSet>,
    pub hosts: Vec<Option<usize>>,
    pub buildings: Vec<(usize, Vec<usize>)>,
}

pub(crate) fn near_threshold(
    ctx: &CandidateContext<'_>,
    progress_window: (f64, f64),
    limit: usize,
    viewers_per_building: usize,
) -> Option<NearThreshold> {
    if ctx.incumbent.is_empty() {
        return None;
    }
    let data = ctx.coverage;
    let goals = CoverageGoals::new(&data.perimeters, ctx.instance.tau);
    let views: Vec<_> = ctx
        .incumbent
        .par_iter()
        .map(|&antenna| {
            let host = ctx
                .evaluator
                .snap(antenna, ANTENNA_SNAP_DIST)
                .map(|(building, _, _)| building);
            (host, ctx.evaluator.antenna_view(antenna).covered)
        })
        .collect();
    let mut acc = vec![IntervalSet::new(); goals.needs.len()];
    let mut hosts = Vec::with_capacity(views.len());
    for (host, view) in &views {
        hosts.push(*host);
        for (building, set) in view {
            acc[*building as usize].union_with(set);
        }
    }

    let (low, high) = progress_window;
    let mut ranked: Vec<_> = acc
        .iter()
        .enumerate()
        .filter_map(|(building, set)| {
            let need = goals.needs[building];
            if need <= 0.0 {
                return None;
            }
            let progress = set.measure() / need;
            (low..high)
                .contains(&progress)
                .then_some((progress, building))
        })
        .collect();
    if limit < ranked.len() {
        ranked.select_nth_unstable_by(limit, |a, b| b.0.total_cmp(&a.0));
        ranked.truncate(limit);
    }
    let buildings = ranked
        .into_iter()
        .map(|(_, building)| {
            let mut viewers = data.by_building[building].clone();
            let keep = viewers_per_building.min(viewers.len());
            if keep < viewers.len() {
                viewers.select_nth_unstable_by(keep, |a, b| b.1.total_cmp(&a.1));
                viewers.truncate(keep);
            }
            let mut viewer_buildings: Vec<usize> = viewers
                .into_iter()
                .map(|(candidate, _)| data.candidates[candidate].building as usize)
                .collect();
            viewer_buildings.sort_unstable();
            viewer_buildings.dedup();
            (building, viewer_buildings)
        })
        .collect();

    Some(NearThreshold {
        goals,
        acc,
        hosts,
        buildings,
    })
}
