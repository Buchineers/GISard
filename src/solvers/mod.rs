mod exact_cover;
mod greedy_coverage;
mod lns;
mod simulated_annealing;
mod swap_refine;
mod window_certify;

pub use exact_cover::{Atom, AtomModel, Cuts, ExactCoverSolver, ExactOutcome, atoms_of};
pub use greedy_coverage::{CoverabilityGreedySolver, GreedyCoverageSolver, select_antennas};
pub use lns::{
    LnsOptions, LnsProgress, LnsSeedStrategy, LnsSolver, select_lns_antennas,
    select_lns_antennas_resuming, select_lns_antennas_until, select_lns_antennas_with_progress,
};
pub(crate) use simulated_annealing::annealing_seed_indices;
pub use simulated_annealing::{
    AnnealingProgress, AnnealingReporter, AnnealingTimedOptions, SimulatedAnnealingConfig,
    SimulatedAnnealingSolver, select_annealed_antennas, select_annealed_antennas_until,
    select_annealed_antennas_until_with_progress,
};
pub use window_certify::{
    Shard, WindowCentres, WindowOptions, WindowReport, WindowResult, apply_window_results,
    certify_windows, repair_windows,
};
