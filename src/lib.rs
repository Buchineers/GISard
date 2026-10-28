#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod antennas;
pub mod campaign;
pub mod candidates;
pub mod coverage;
#[cfg(feature = "visualizer")]
pub mod inspector;
pub mod sampling;
pub mod solvers;
