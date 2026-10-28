use std::sync::Arc;

use rayon::prelude::*;

use crate::types::{Instance, Solution};

pub type Reconstructor = Box<dyn FnOnce(Vec<Solution>) -> Solution + Send>;

pub trait PipelineStep: Send + Sync {
    fn apply(&self, instance: Instance) -> (Vec<Instance>, Reconstructor);
}

pub trait Solver: Send + Sync {
    fn solve(&self, instance: Instance) -> Solution;
}

#[derive(Clone, Copy)]
pub enum PipelineEvent<'a> {
    Instance(&'a Instance),
    Step { index: usize, parts: &'a [Instance] },
    Solved(&'a Solution),
}

pub trait PipelineObserver: Send + Sync {
    fn on_event(&self, event: PipelineEvent);
}

pub struct Pipeline {
    steps: Vec<Box<dyn PipelineStep>>,
    solver: Box<dyn Solver>,
    observers: Vec<Arc<dyn PipelineObserver>>,
}

impl Pipeline {
    pub fn new(solver: Box<dyn Solver>) -> Self {
        Self {
            steps: Vec::new(),
            solver,
            observers: Vec::new(),
        }
    }

    pub fn add_observer(mut self, observer: Arc<dyn PipelineObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    pub fn add_step(mut self, step: Box<dyn PipelineStep>) -> Self {
        self.steps.push(step);
        self
    }

    pub fn run(&self, instance: Instance) -> Solution {
        self.emit(PipelineEvent::Instance(&instance));
        let mut solutions = self.run_batch_from_step(vec![instance], 0);
        solutions
            .pop()
            .expect("a batch of one instance yields one solution")
    }

    fn run_batch_from_step(&self, instances: Vec<Instance>, step_index: usize) -> Vec<Solution> {
        if step_index >= self.steps.len() {
            let solutions: Vec<Solution> = instances
                .into_par_iter()
                .map(|inst| self.solver.solve(inst))
                .collect();
            for sol in &solutions {
                self.emit(PipelineEvent::Solved(sol));
            }
            return solutions;
        }

        let step = &self.steps[step_index];
        let results: Vec<_> = instances
            .into_par_iter()
            .map(|inst| step.apply(inst))
            .collect();
        let mut all_subs = Vec::new();
        let mut group_sizes = Vec::new();
        let mut reconstructors = Vec::new();
        for (subs, recon) in results {
            group_sizes.push(subs.len());
            all_subs.extend(subs);
            reconstructors.push(recon);
        }
        self.emit(PipelineEvent::Step {
            index: step_index,
            parts: &all_subs,
        });
        let all_solutions = self.run_batch_from_step(all_subs, step_index + 1);
        let mut sol_iter = all_solutions.into_iter();
        reconstructors
            .into_iter()
            .zip(group_sizes)
            .map(|(recon, size)| {
                let group: Vec<_> = sol_iter.by_ref().take(size).collect();
                recon(group)
            })
            .collect()
    }

    fn emit(&self, event: PipelineEvent) {
        for o in &self.observers {
            o.on_event(event);
        }
    }
}
