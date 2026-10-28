use framework::{CoverageGoals, IntervalSet};
use rayon::prelude::*;

use crate::coverage::CoverageData;
use crate::sampling::CandidateKind;

const EPS: f64 = 1e-9;

pub(super) fn refine(
    data: &CoverageData,
    goals: &CoverageGoals,
    chosen: &mut [usize],
    max_passes: usize,
    add_candidate_limit: usize,
) {
    if chosen.is_empty() || chosen.len() >= data.candidates.len() {
        return;
    }
    let add_index = RefineIndex::new(data, goals, add_candidate_limit);
    let mut used = used_mask(data.candidates.len(), chosen);
    let mut current = accumulated_fitness(data, goals, chosen.iter().copied());

    for _ in 0..max_passes {
        let Some(best) = best_swap(
            data,
            goals,
            chosen,
            &used,
            current,
            add_index.as_ref(),
            add_candidate_limit,
        ) else {
            break;
        };
        let removed = chosen[best.remove_pos];
        chosen[best.remove_pos] = best.add;
        used[removed] = false;
        used[best.add] = true;
        current = best.fitness;
    }
}

pub(super) fn used_mask(n: usize, chosen: &[usize]) -> Vec<bool> {
    let mut used = vec![false; n];
    for &i in chosen {
        used[i] = true;
    }
    used
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Fitness {
    pub(super) serviced: usize,
    pub(super) progress: f64,
}

impl Fitness {
    pub(super) fn better_than(self, other: Self) -> bool {
        self.serviced > other.serviced
            || (self.serviced == other.serviced && self.progress > other.progress + EPS)
    }

    pub(super) fn replace_coverage(&mut self, need: f64, old: f64, new: f64) {
        self.progress += need.min(new) - need.min(old);
        match (need > 0.0 && old >= need, need > 0.0 && new >= need) {
            (true, false) => self.serviced -= 1,
            (false, true) => self.serviced += 1,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Swap {
    remove_pos: usize,
    add: usize,
    fitness: Fitness,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct RepairBound {
    pub(super) completions: usize,
    pub(super) progress: f64,
}

impl RepairBound {
    pub(super) fn of(entries: (&[u32], &[f64]), remaining: &[f64]) -> Self {
        let (buildings, measures) = entries;
        let mut bound = Self {
            completions: 0,
            progress: 0.0,
        };
        for (&building, &measure) in buildings.iter().zip(measures) {
            let remaining = remaining[building as usize];
            if remaining > 0.0 {
                bound.progress += remaining.min(measure);
                if measure + EPS >= remaining {
                    bound.completions += 1;
                }
            }
        }
        bound
    }

    pub(super) fn can_qualify(self) -> bool {
        self.completions > 0 || self.progress > EPS
    }

    pub(super) fn optimistic(self, base: Fitness) -> Fitness {
        Fitness {
            serviced: base.serviced + self.completions,
            progress: base.progress + self.progress + EPS,
        }
    }

    pub(super) fn can_beat(self, completions: usize, progress: f64) -> bool {
        self.completions > completions
            || (self.completions == completions && self.progress + EPS >= progress)
    }
}

fn best_swap(
    data: &CoverageData,
    goals: &CoverageGoals,
    chosen: &[usize],
    used: &[bool],
    current: Fitness,
    add_index: Option<&RefineIndex>,
    _add_candidate_limit: usize,
) -> Option<Swap> {
    (0..chosen.len())
        .into_par_iter()
        .filter_map(|remove_pos| {
            let base = Accumulation::without(data, goals, chosen, remove_pos);
            let remaining = &base.remaining(goals);
            match add_index {
                Some(add_index) => add_index
                    .global_rank
                    .par_iter()
                    .copied()
                    .filter(|&add| !used[add])
                    .map(|add| Swap {
                        remove_pos,
                        add,
                        fitness: base.after_adding(goals, &data.visibility[add]),
                    })
                    .reduce_with(best_of),
                None => (0..data.candidates.len())
                    .into_par_iter()
                    .filter(|&add| !used[add])
                    .filter_map(|add| {
                        let bound = RepairBound::of(data.flat.entries(add), remaining);
                        bound
                            .optimistic(base.fitness)
                            .better_than(current)
                            .then(|| Swap {
                                remove_pos,
                                add,
                                fitness: base.after_adding(goals, &data.visibility[add]),
                            })
                    })
                    .reduce_with(best_of),
            }
        })
        .reduce_with(best_of)
        .filter(|swap| swap.fitness.better_than(current))
}

struct RefineIndex {
    global_rank: Vec<usize>,
}

impl RefineIndex {
    fn new(data: &CoverageData, goals: &CoverageGoals, add_candidate_limit: usize) -> Option<Self> {
        if add_candidate_limit == 0 || add_candidate_limit >= data.candidates.len() {
            return None;
        }
        let single_scores: Vec<f64> = data
            .visibility
            .par_iter()
            .map(|view| {
                view.iter()
                    .map(|(b, set)| goals.needs[*b as usize].min(set.measure()))
                    .sum()
            })
            .collect();
        let is_event = |kind| kind == CandidateKind::Event;
        let mut global_rank =
            ranked_candidates(data, &single_scores, add_candidate_limit, |kind| {
                !is_event(kind)
            });
        if data.candidates.iter().any(|c| is_event(c.kind)) {
            global_rank.extend(ranked_candidates(
                data,
                &single_scores,
                add_candidate_limit / 4,
                is_event,
            ));
            global_rank.sort_unstable();
            global_rank.dedup();
            global_rank.sort_unstable_by(|&a, &b| {
                single_scores[b]
                    .total_cmp(&single_scores[a])
                    .then(a.cmp(&b))
            });
        }
        Some(Self { global_rank })
    }
}

fn ranked_candidates(
    data: &CoverageData,
    single_scores: &[f64],
    limit: usize,
    select: impl Fn(CandidateKind) -> bool,
) -> Vec<usize> {
    let mut rank: Vec<usize> = data
        .candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| select(c.kind).then_some(i))
        .collect();
    if limit < rank.len() {
        rank.select_nth_unstable_by(limit, |&a, &b| {
            single_scores[b].total_cmp(&single_scores[a])
        });
        rank.truncate(limit);
    }
    rank.sort_unstable_by(|&a, &b| {
        single_scores[b]
            .total_cmp(&single_scores[a])
            .then(a.cmp(&b))
    });
    rank
}

fn best_of(a: Swap, b: Swap) -> Swap {
    if b.fitness.better_than(a.fitness) {
        b
    } else {
        a
    }
}

pub(super) struct Accumulation {
    pub(super) acc: Vec<IntervalSet>,
    pub(super) covered: Vec<f64>,
    serviced: Vec<bool>,
    pub(super) fitness: Fitness,
}

impl Accumulation {
    fn without(
        data: &CoverageData,
        goals: &CoverageGoals,
        chosen: &[usize],
        skip_pos: usize,
    ) -> Self {
        let indices = chosen
            .iter()
            .enumerate()
            .filter_map(|(pos, &i)| (pos != skip_pos).then_some(i));
        accumulated(data, goals, indices)
    }

    pub(super) fn after_adding(
        &self,
        goals: &CoverageGoals,
        view: &[(u32, IntervalSet)],
    ) -> Fitness {
        let mut fitness = self.fitness;
        for (b, set) in view {
            let b = *b as usize;
            let union = self.acc[b].union_measure(set);
            fitness.replace_coverage(goals.needs[b], self.covered[b], union);
        }
        fitness
    }

    pub(super) fn add(&mut self, goals: &CoverageGoals, view: &[(u32, IntervalSet)]) {
        for (b, set) in view {
            let b = *b as usize;
            self.acc[b].union_with(set);
            let covered = self.acc[b].measure();
            self.fitness
                .replace_coverage(goals.needs[b], self.covered[b], covered);
            self.covered[b] = covered;
            self.serviced[b] = goals.needs[b] > 0.0 && covered >= goals.needs[b];
        }
    }

    pub(super) fn covered(&self, building: usize) -> f64 {
        self.covered[building]
    }

    pub(super) fn is_serviced(&self, building: usize) -> bool {
        self.serviced[building]
    }

    pub(super) fn remaining(&self, goals: &CoverageGoals) -> Vec<f64> {
        self.covered
            .iter()
            .zip(&goals.needs)
            .enumerate()
            .map(|(b, (&covered, &need))| {
                if self.serviced[b] {
                    0.0
                } else {
                    (need - covered).max(0.0)
                }
            })
            .collect()
    }

    pub(super) fn after_adding_two(
        &self,
        goals: &CoverageGoals,
        first: &[(u32, IntervalSet)],
        second: &[(u32, IntervalSet)],
    ) -> Fitness {
        let mut fitness = self.fitness;
        let (mut i, mut j) = (0, 0);
        while i < first.len() || j < second.len() {
            let building = match (first.get(i), second.get(j)) {
                (Some((a, _)), Some((b, _))) => (*a).min(*b),
                (Some((a, _)), None) => *a,
                (None, Some((b, _))) => *b,
                (None, None) => break,
            };
            let b = building as usize;
            let mut union = self.acc[b].clone();
            if first.get(i).is_some_and(|(index, _)| *index == building) {
                union.union_with(&first[i].1);
                i += 1;
            }
            if second.get(j).is_some_and(|(index, _)| *index == building) {
                union.union_with(&second[j].1);
                j += 1;
            }
            let covered = union.measure();
            fitness.replace_coverage(goals.needs[b], self.covered[b], covered);
        }
        fitness
    }
}

pub(super) fn accumulated_fitness<I>(
    data: &CoverageData,
    goals: &CoverageGoals,
    indices: I,
) -> Fitness
where
    I: IntoIterator<Item = usize>,
{
    accumulated(data, goals, indices).fitness
}

pub(super) fn accumulated<I>(data: &CoverageData, goals: &CoverageGoals, indices: I) -> Accumulation
where
    I: IntoIterator<Item = usize>,
{
    let mut acc = vec![IntervalSet::new(); goals.needs.len()];
    for i in indices {
        for (b, set) in &data.visibility[i] {
            acc[*b as usize].union_with(set);
        }
    }
    let covered: Vec<f64> = acc.iter().map(IntervalSet::measure).collect();
    let serviced: Vec<bool> = covered
        .iter()
        .zip(&goals.needs)
        .map(|(&c, &need)| need > 0.0 && c >= need)
        .collect();
    let fitness = Fitness {
        serviced: serviced.iter().filter(|&&s| s).count(),
        progress: goals.progress(&acc),
    };
    Accumulation {
        acc,
        covered,
        serviced,
        fitness,
    }
}
