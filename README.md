# GISard — GIS Cup 2026

Antenna placement on building boundaries for the ACM SIGSPATIAL GIS Cup 2026,
by team Buchineers. The method and results are described in our paper:

> C. Inhetveen, M. Hagedoorn, M. Mennemann, M. M. Hille, K. Buchin.
> *Distributed Annealing with Exact Local Certification for Antenna Placement
> on Building Boundaries.* SIGSPATIAL '26.
> [doi:10.1145/3841645.3850167](https://doi.org/10.1145/3841645.3850167)

## Build

Rust edition 2024, toolchain 1.85+. No system dependencies; HiGHS is vendored
and built from source.

```bash
cargo build --release
cargo test --workspace
```

Optional features, both off by default: `--features visualizer` (solution
explorer, `sandbox`) and `--features dashboard` (campaign TUI,
`campaign_dashboard`).

### Static build for LiDO3

The cluster has no Rust toolchain and an old glibc, so the binaries are
cross-compiled to a static musl target (needs `cargo-zigbuild` and `zig`) and
synced over:

```bash
make build-cluster
make sync REMOTE_DIR=/work/$USER/gisard
```

## Dataset

The competition dataset is not redistributed here. Obtain it from the GIS Cup
2026 organizers and place it at `instances/GIS-cup-competition-dataset.geojson`
(locally and under the campaign root on the cluster).

## Single sub-problem

```bash
# sa_solve <instance.geojson> <k> <tau> [spacing] [iterations] [proposal_samples] [seed] ...
cargo run --release --bin sa_solve -- \
    instances/GIS-cup-competition-dataset.geojson 9 0.32 25.64 200000

# score <instance.geojson> <antennas.txt> [tau...]
cargo run --release --bin score -- \
    instances/GIS-cup-competition-dataset.geojson antennas.txt 0.32
```

`sa_solve` writes one `x,y` per line; `score` reports the serviced buildings
under our exact evaluator. At `k=484` on the full dataset a trial takes tens of
minutes on 4 cores, so use a smaller `k` for a smoke test.

## Full campaign on LiDO3

One command queues the whole chain (probe → memory check → annealing array →
polish → harvest):

```bash
./lido/launch_sa_campaign.sh \
  --campaign  /work/$USER/gisard/<campaign-dir> \
  --dataset   /work/$USER/gisard/instances/GIS-cup-competition-dataset.geojson \
  --scenarios 0.32:9,0.32:49,0.32:484,0.49:9,0.49:49,0.49:484,0.68:9,0.68:49,0.68:484 \
  --deadline  '2026-08-16 16:00:00 UTC'
```

Monitoring:

```bash
./lido/status_sa_campaign.sh <campaign-dir>   # scenario table, rungs, allocation
bin/campaign_dashboard <campaign-dir>         # live TUI
cat <campaign-dir>/launch.env                 # job ids, source revision, fingerprints
```

The harvest writes `<campaign-dir>/submission/solution.txt`.

### Window repair

Window repair runs alongside the annealing array on separate nodes.
`polish_waves.sh` sweeps repeatedly until a UTC cutoff and escalates to
stronger windows when a wave finds nothing (`--help` lists all options);
`launch_polish_fanout.sh` queues a single wave:

```bash
nohup ./lido/polish_waves.sh --campaign <campaign-dir> --until 14:00 \
    > logs/waves.log 2>&1 &
./lido/launch_polish_fanout.sh --campaign <campaign-dir> --nodes <n>
```

The underlying commands, e.g. for running a shard by hand:

```bash
campaign certify-shard <campaign-dir> --scenario t032-k484 --shard 7 --of 3 \
    --out shards/t032-k484-0007.json --antennas 1 --radius 60 --centres antenna
campaign polish-merge <campaign-dir> --shards shards/
```

## Official scoring

The building ids claimed in the submission come from the official evaluator
([alowe/gis-cup-2026-evaluator](https://github.com/alowe/gis-cup-2026-evaluator);
run `pnpm install` in it first). Run it locally on the harvested file:

```bash
GIS_CUP_EVALUATOR_DIR=/path/to/gis-cup-2026-evaluator \
  official/evaluate_submission.sh \
  instances/GIS-cup-competition-dataset.geojson solution.rust.txt solution.txt
```

For a single sub-problem, use
`official/score_solution.sh <dataset> <antennas.txt> <tau> <k> <output.txt>`
with the same environment variable.
