use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use framework::parse_instance;
use gisard::campaign::{
    AshaPolicy, BaselinePolicy, CampaignEvent, FakeRunner, LnsRunner, Manifest, Options, Policy,
    PolishOptions, Probe, ProbeOptions, Queue, SaPolicy, SaProbe, SaProbeOptions, SaRunner,
    Scenario, TimedResult, TrialRunner, WorkerHeartbeat, default_sa_tiers, harvest, now_unix,
    polish, probe, read_events, sa_memory_check, sa_probe, smoke_sa_tiers, work,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [command, dir, rest @ ..] = args.as_slice() else {
        usage();
        return ExitCode::FAILURE;
    };
    let dir = PathBuf::from(dir);

    match command.as_str() {
        "init" => init(&dir, rest),
        "probe" => run_probe(&dir, rest),
        "sa-probe" => run_sa_probe(&dir, rest),
        "sa-memory-check" => run_sa_memory_check(&dir, rest),
        "work" => run_worker(&dir, rest),
        "polish" => run_polish(&dir, rest),
        "certify-shard" => run_certify_shard(&dir, rest),
        "polish-merge" => run_polish_merge(&dir, rest),
        "status" => status(&dir, rest),
        "events" => events(&dir, rest),
        "harvest" => return harvest_cmd(&dir, rest),
        other => {
            eprintln!("unknown command: {other}");
            usage();
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn usage() {
    eprintln!("usage:");
    eprintln!("  campaign init <dir> --dataset <path> --scenarios <tau>:<k>,...");
    eprintln!("                      [--deadline <unix> | --duration-hours <hours>]");
    eprintln!("                      [--reserve <seconds>] [--spacings <a,b,...>]");
    eprintln!("  campaign work <dir> --worker <id> [--fake-solver [seconds]] [--max-trials <n>]");
    eprintln!("                      [--solver lns|sa] [--policy asha|baseline]");
    eprintln!("                      [--budgets <seconds,...>] [--chains <n>] [--spacing <n>]");
    eprintln!("                      [--pair-probability <p>] [--target-probability <p>]");
    eprintln!(
        "  campaign probe <dir> [--spacings <a,b,c>] [--mem-limit-mb <n>] [--iterations <n>]"
    );
    eprintln!("  campaign sa-probe <dir> [--spacings <a,b,c>]");
    eprintln!("                            [--trial-seconds <n>]");
    eprintln!("                            [--nodes <n>]");
    eprintln!("      [--node-memory-mb <n>] [--node-cores <n>] [--max-workers <n>]");
    eprintln!("      [--portfolio | --smoke-portfolio]");
    eprintln!("  campaign sa-memory-check <dir> --spacing <n> --chains <n> [--seconds <n>]");
    eprintln!("                                  [--metrics-dir <path>]");
    eprintln!("  campaign polish <dir> [--radius <units>] [--windows <n>] [--rounds <n>]");
    eprintln!(
        "  campaign certify-shard <dir> --scenario <id> --shard <i> --of <n> --out <file> \
         [--radius <units>] [--windows <n>] [--antennas <n>] [--time-limit <s>] \
         [--spacing <units>] [--centres antenna|open|both]"
    );
    eprintln!("  campaign polish-merge <dir> --shards <dir> [--scenario <id>]");
    eprintln!("  campaign status <dir> [--claims] [--compact] [--cache <path>]");
    eprintln!(
        "  campaign events <dir> [--limit <n>] [--since-seconds <n>] [--scenario <id>] [--json]"
    );
    eprintln!("  campaign harvest <dir> [--out <path>]");
}

fn flags(rest: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i < rest.len() {
        let key = rest[i].trim_start_matches("--").to_string();
        let value = rest
            .get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .cloned()
            .unwrap_or_default();
        i += if value.is_empty() { 1 } else { 2 };
        out.insert(key, value);
    }
    out
}

fn parse_scenarios(value: &str) -> Vec<Scenario> {
    value
        .split(',')
        .map(|pair| {
            let (tau, k) = pair.split_once(':').expect("scenarios look like <tau>:<k>");
            Scenario::new(
                tau.parse().expect("invalid tau"),
                k.parse().expect("invalid k"),
            )
        })
        .collect()
}

fn init(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let dataset = flags
        .get("dataset")
        .expect("init needs --dataset <path>")
        .clone();

    let scenarios = parse_scenarios(
        flags
            .get("scenarios")
            .expect("init needs --scenarios <tau>:<k>,..."),
    );

    let deadline = flags.get("deadline").map(|value| {
        value
            .parse::<u64>()
            .expect("invalid deadline: expected Unix seconds")
    });
    let duration_hours = flags
        .get("duration-hours")
        .map(|value| value.parse::<f64>().expect("invalid duration-hours"));
    let duration_seconds = flags
        .get("duration-seconds")
        .map(|value| value.parse::<u64>().expect("invalid duration-seconds"));
    assert!(
        usize::from(deadline.is_some())
            + usize::from(duration_hours.is_some())
            + usize::from(duration_seconds.is_some())
            <= 1,
        "choose at most one deadline mode: --deadline, --duration-hours, or --duration-seconds"
    );
    let duration_seconds = duration_seconds.unwrap_or_else(|| {
        duration_hours.map_or(0, |hours| {
            assert!(
                hours.is_finite() && hours > 0.0,
                "duration-hours must be finite and positive"
            );
            let seconds = (hours * 3600.0).round() as u64;
            assert!(seconds > 0, "duration-hours rounds to zero seconds");
            seconds
        })
    });
    let reserve_seconds = flags
        .get("reserve")
        .map_or(1800, |value| value.parse().expect("invalid reserve"));
    if duration_seconds > 0 {
        assert!(
            reserve_seconds < duration_seconds,
            "reserve must be shorter than the relative duration"
        );
    }
    let buildings = parse_instance(&dataset, 1, 0.5).buildings.len();
    let manifest = Manifest {
        dataset: PathBuf::from(dataset),
        scenarios,
        buildings,
        start_unix: if duration_seconds > 0 { 0 } else { now_unix() },
        deadline_unix: deadline.unwrap_or(0),
        reserve_seconds,
        duration_seconds,
        spacings: flags.get("spacings").map_or_else(
            || vec![8.0],
            |v| {
                v.split(',')
                    .map(|s| s.parse().expect("invalid spacing"))
                    .collect()
            },
        ),
    };

    std::fs::create_dir_all(dir).expect("failed to create the campaign directory");
    Manifest::reset_clock(dir);
    Queue::new(dir).init().expect("failed to lay out the queue");
    manifest.save(&dir.join("manifest.json"));
    println!(
        "initialised {} with {} scenarios",
        dir.display(),
        manifest.scenarios.len()
    );
    for s in &manifest.scenarios {
        println!("  {}  tau {}  k {}", s.id, s.tau, s.k);
    }
    if duration_seconds > 0 {
        println!(
            "relative deadline: {:.2}h after the first worker starts ({}s reserve)",
            duration_seconds as f64 / 3600.0,
            reserve_seconds
        );
    } else if let Some(deadline) = deadline {
        println!("absolute deadline: Unix {deadline} ({reserve_seconds}s reserve)");
    } else {
        println!("no deadline configured");
    }
}

fn run_probe(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let manifest = Manifest::load(&dir.join("manifest.json"));
    let dataset = manifest.dataset_path(dir);
    let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);

    let mut options = ProbeOptions::default();
    if let Some(v) = flags.get("spacings") {
        options.spacings = v
            .split(',')
            .map(|s| s.parse().expect("invalid spacing"))
            .collect();
    }
    if let Some(v) = flags.get("mem-limit-mb") {
        options.memory_limit_mb = v.parse().expect("invalid mem-limit-mb");
    }
    if let Some(v) = flags.get("iterations") {
        options.iterations = v.parse().expect("invalid iterations");
    }

    let result = probe(&instance, &manifest, &options);
    println!(
        "{} buildings, {} vertices",
        result.buildings, result.vertices
    );
    println!(
        "\n{:<10}{:<14}{:<14}{:<12}fits",
        "spacing", "candidates", "build (s)", "memory (MB)"
    );
    for s in &result.spacings {
        println!(
            "{:<10}{:<14}{:<14.1}{:<12.0}{}{}",
            s.spacing,
            s.candidates,
            s.build_seconds,
            s.memory_mb,
            if s.fits { "yes" } else { "NO" },
            if s.measured { "  (measured)" } else { "" },
        );
    }
    println!(
        "\nrecommended spacing: {} (limit {} MB)",
        result.recommended_spacing, result.memory_limit_mb
    );

    println!(
        "\n{:<14}{:<8}{:<14}{:<10}ladder",
        "scenario", "k", "s/iteration", "serviced"
    );
    let full = gisard::campaign::RUNGS.to_vec();
    for s in &result.scenarios {
        println!(
            "{:<14}{:<8}{:<14.3}{:<10}{:?}{}",
            s.id,
            s.k,
            s.iteration_seconds,
            s.serviced,
            s.rungs,
            if s.rungs == full { "" } else { "  (trimmed)" },
        );
    }
    if result.scenarios.iter().any(|s| s.rungs != full) {
        println!("\ntrimmed ladders: those top rungs would not finish in time");
    }

    result.save(&dir.join("probe.json"));
    println!("wrote {}", dir.join("probe.json").display());
}

fn run_sa_probe(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let manifest = Manifest::load(&dir.join("manifest.json"));
    let dataset = manifest.dataset_path(dir);
    let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);
    let mut options = SaProbeOptions::default();
    if let Some(value) = flags.get("spacings") {
        options.spacings = value
            .split(',')
            .map(|spacing| spacing.parse().expect("invalid spacing"))
            .collect();
    }
    if let Some(value) = flags.get("trial-seconds") {
        options.trial_seconds = value.parse().expect("invalid trial-seconds");
    }
    if let Some(value) = flags.get("node-memory-mb") {
        options.node_memory_mb = value.parse().expect("invalid node-memory-mb");
    }
    if let Some(value) = flags.get("node-cores") {
        options.node_cores = value.parse().expect("invalid node-cores");
    }
    if let Some(value) = flags.get("max-workers") {
        options.max_workers = value.parse().expect("invalid max-workers");
    }
    if let Some(value) = flags.get("chains") {
        options.chains_per_worker = value.parse().expect("invalid chains");
    }
    assert!(
        !(flags.contains_key("portfolio") && flags.contains_key("smoke-portfolio")),
        "choose only one SA portfolio"
    );
    if flags.contains_key("smoke-portfolio") {
        options.tiers = smoke_sa_tiers();
    } else if flags.contains_key("portfolio") {
        options.tiers = default_sa_tiers();
    }
    if let Some(value) = flags.get("nodes") {
        let nodes: usize = value.parse().expect("invalid nodes");
        assert!(nodes > 0, "nodes must be positive");
        assert!(
            !options.tiers.is_empty(),
            "--nodes requires an SA portfolio"
        );
        for tier in &mut options.tiers {
            tier.nodes = nodes;
        }
    }

    let result = sa_probe(&instance, &manifest, &options);
    println!(
        "{} buildings, {} vertices\n",
        result.buildings, result.vertices
    );
    for spacing in &result.spacings {
        println!(
            "spacing {:<6} {:>8} candidates, {:>7.1}s build, {:>8.0} MB{}",
            spacing.spacing,
            spacing.candidates,
            spacing.build_seconds,
            spacing.matrix_memory_mb,
            if spacing.fits { "" } else { "  DOES NOT FIT" },
        );
        for scenario in &spacing.scenarios {
            println!(
                "  {:<14} score {:<8} in {:.1}s",
                scenario.id, scenario.serviced, scenario.seconds
            );
        }
    }
    if result.tiers.is_empty() {
        println!(
            "\nrecommended: spacing {}, {} workers/node x {} chains ({} CPUs/worker)",
            result.recommended_spacing,
            result.topology.workers_per_node,
            result.topology.chains_per_worker,
            result.topology.cpus_per_worker,
        );
    } else {
        println!("\ncalibration:");
        for point in &result.calibration {
            println!(
                "  spacing {:<10.5} {:>9} candidates, {:>8.0} MB, {:>7.1}s build",
                point.spacing, point.candidates, point.matrix_memory_mb, point.build_seconds,
            );
        }
        println!("\nproduction portfolio:");
        for tier in &result.tiers {
            println!(
                "  {:<4} {:>3} nodes x {} workers x {} chains: spacing {:<10.5} {:>8.0}/{:.0} MB per matrix, {:>8.0} MB node headroom",
                tier.id,
                tier.nodes,
                tier.workers_per_node,
                tier.chains_per_worker,
                tier.spacing,
                tier.matrix_memory_mb,
                tier.matrix_limit_mb,
                tier.node_headroom_mb,
            );
        }
        println!("\nASHA densities: {:?}", result.search_spacings);
    }
    result.save(&dir.join("sa-probe.json"));
    result.save_portfolio(&dir.join("sa-portfolio.tsv"));
    result.save_densities(&dir.join("sa-densities.tsv"));
    println!("wrote {}", dir.join("sa-probe.json").display());
    if !result.tiers.is_empty() {
        println!("wrote {}", dir.join("sa-portfolio.tsv").display());
        println!("wrote {}", dir.join("sa-densities.tsv").display());
    }
}

fn run_sa_memory_check(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let manifest = Manifest::load(&dir.join("manifest.json"));
    let dataset = manifest.dataset_path(dir);
    let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);
    let spacing = flags
        .get("spacing")
        .expect("sa-memory-check needs --spacing")
        .parse::<f64>()
        .expect("invalid spacing");
    let chains = flags
        .get("chains")
        .expect("sa-memory-check needs --chains")
        .parse::<usize>()
        .expect("invalid chains");
    let seconds = flags
        .get("seconds")
        .map_or(60, |value| value.parse().expect("invalid seconds"));
    let seed = flags
        .get("seed")
        .map(|value| value.parse().expect("invalid seed"))
        .or_else(|| {
            std::env::var("SLURM_PROCID")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0);
    let scenario = manifest
        .scenarios
        .iter()
        .max_by_key(|scenario| scenario.k)
        .expect("memory check needs a scenario");
    let result = sa_memory_check(&instance, scenario, spacing, chains, seconds, seed);
    println!(
        "memory check: spacing {:.8}, {} chains, {} candidates, guarded matrix {:.0} MB, RSS after build {:.0} MB, process peak {:.0} MB",
        result.spacing,
        result.chains,
        result.candidates,
        result.matrix_memory_mb,
        result.rss_after_build_mb,
        result.peak_rss_mb,
    );
    println!(
        "  {} score {} after {:.1}s; matrix build {:.1}s",
        result.scenario, result.serviced, result.run_seconds, result.build_seconds
    );
    if let Some(metrics_dir) = flags.get("metrics-dir") {
        let metrics_dir = PathBuf::from(metrics_dir);
        std::fs::create_dir_all(&metrics_dir).unwrap_or_else(|error| {
            panic!(
                "failed to create metrics directory {}: {error}",
                metrics_dir.display()
            )
        });
        let task = std::env::var("SLURM_PROCID").unwrap_or_else(|_| std::process::id().to_string());
        let path = metrics_dir.join(format!("task-{task}.tsv"));
        let temporary = metrics_dir.join(format!(".task-{task}-{}.tmp", std::process::id()));
        let body = format!(
            "{:.3}\t{:.3}\t{}\t{:.3}\t{:.3}\n",
            result.matrix_memory_mb,
            result.peak_rss_mb,
            result.candidates,
            result.build_seconds,
            result.run_seconds,
        );
        std::fs::write(&temporary, body)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", temporary.display()));
        std::fs::rename(&temporary, &path)
            .unwrap_or_else(|error| panic!("failed to publish {}: {error}", path.display()));
        println!("wrote {}", path.display());
    }
}

fn run_worker(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let solver = flags.get("solver").map_or("lns", String::as_str);
    assert!(matches!(solver, "lns" | "sa"), "unknown --solver: {solver}");
    let worker = flags
        .get("worker")
        .cloned()
        .unwrap_or_else(|| std::process::id().to_string());
    let heartbeat = WorkerHeartbeat::start(dir, &worker);
    let max_trials = flags
        .get("max-trials")
        .map_or(usize::MAX, |v| v.parse().expect("invalid max-trials"));
    let measured = Probe::load(&dir.join("probe.json"));
    let measured_sa = SaProbe::load(&dir.join("sa-probe.json"));
    let forced_sa_spacing = flags
        .get("spacing")
        .map(|value| value.parse().expect("invalid spacing"));
    let sa_spacings = forced_sa_spacing.map_or_else(
        || {
            measured_sa
                .as_ref()
                .map_or_else(|| vec![8.0], SaProbe::production_spacings)
        },
        |spacing| vec![spacing],
    );
    if solver == "lns"
        && let Some(p) = &measured
    {
        println!("probe: spacing {}", p.recommended_spacing);
        for s in &p.scenarios {
            println!("  {:<14} ladder {:?}", s.id, s.rungs);
        }
    }
    if solver == "sa"
        && let Some(p) = &measured_sa
    {
        println!(
            "SA probe: production spacing portfolio {:?} ({} workers/node x {} chains)",
            sa_spacings, p.topology.workers_per_node, p.topology.chains_per_worker
        );
    }

    let asha;
    let baseline;
    let sa_policy;
    let policy: &dyn Policy = match (solver, flags.get("policy").map(String::as_str)) {
        ("lns", Some("baseline")) => {
            baseline = BaselinePolicy {
                max_trials,
                ..Default::default()
            };
            &baseline
        }
        ("lns", Some("asha") | None) => {
            let mut built = AshaPolicy {
                max_trials,
                ..Default::default()
            };
            if let Some(p) = &measured {
                built = built.with_probe(p);
                built.max_trials = max_trials;
            }
            asha = built;
            &asha
        }
        ("sa", Some("sa") | None) => {
            let mut built = SaPolicy {
                max_trials,
                ..Default::default()
            }
            .with_spacings(sa_spacings.clone());
            if let Some(value) = flags.get("budgets") {
                built = built.with_budgets(
                    value
                        .split(',')
                        .map(|budget| budget.parse().expect("invalid SA budget"))
                        .collect(),
                );
            }
            if let Some(value) = flags.get("cohort-size") {
                built = built.with_cohort_size(value.parse().expect("invalid SA cohort size"));
            }
            if let Some(value) = flags.get("pair-probability") {
                built = built
                    .with_pair_probability(value.parse().expect("invalid SA pair probability"));
            }
            if let Some(value) = flags.get("target-probability") {
                built = built
                    .with_target_probability(value.parse().expect("invalid SA target probability"));
            }
            sa_policy = built;
            &sa_policy
        }
        (_, Some(other)) => panic!("policy {other} does not apply to solver {solver}"),
        _ => unreachable!(),
    };

    let mut fake;
    let mut lns;
    let mut sa;
    let runner: &mut dyn TrialRunner = if let Some(seconds) = flags.get("fake-solver") {
        fake = FakeRunner {
            seconds: seconds.parse().unwrap_or(0.0),
        };
        &mut fake
    } else {
        let manifest = Manifest::load(&dir.join("manifest.json"));
        let dataset = manifest.dataset_path(dir);
        let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);
        match solver {
            "lns" => {
                lns = LnsRunner::new(instance);
                &mut lns
            }
            "sa" => {
                let chains = flags
                    .get("chains")
                    .map(|value| value.parse().expect("invalid chains"))
                    .or_else(|| {
                        std::env::var("SLURM_CPUS_PER_TASK")
                            .ok()
                            .and_then(|value| value.parse().ok())
                    })
                    .or_else(|| {
                        measured_sa
                            .as_ref()
                            .map(|probe| probe.topology.chains_per_worker)
                    })
                    .unwrap_or(4);
                let build_seconds = measured_sa
                    .as_ref()
                    .map(|probe| {
                        probe
                            .spacings
                            .iter()
                            .map(|row| row.build_seconds)
                            .fold(0.0, f64::max)
                    })
                    .unwrap_or(120.0);
                sa = SaRunner::new(instance, chains).with_build_estimate(build_seconds);
                &mut sa
            }
            _ => unreachable!(),
        }
    };

    let stats = work(dir, &worker, runner, policy, heartbeat);
    println!(
        "worker {worker}: {} trials, {} claims lost{}",
        stats.completed,
        stats.claims_lost,
        if stats.stopped_for_deadline {
            ", stopped for the deadline"
        } else {
            ""
        }
    );
}

fn shard_instance(dir: &Path) -> (Manifest, framework::Instance) {
    let manifest = Manifest::load(&dir.join("manifest.json"));
    let dataset = manifest.dataset_path(dir);
    let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);
    (manifest, instance)
}

fn shard_options(flags: &BTreeMap<String, String>) -> gisard::campaign::ShardOptions {
    let mut options = gisard::campaign::ShardOptions::default();
    if let Some(v) = flags.get("radius") {
        options.radius = v.parse().expect("invalid radius");
    }
    if let Some(v) = flags.get("windows") {
        options.windows = Some(v.parse().expect("invalid windows"));
    }
    if let Some(v) = flags.get("antennas") {
        options.antennas_per_window = v.parse().expect("invalid antennas");
    }
    if let Some(v) = flags.get("time-limit") {
        options.time_limit = v.parse().expect("invalid time limit");
    }
    if let Some(v) = flags.get("spacing") {
        options.spacing = v.parse().expect("invalid spacing");
    }
    if let Some(v) = flags.get("centres") {
        options.centres =
            gisard::campaign::ShardCentres::parse(v).expect("invalid centres: antenna|open|both");
    }
    options
}

fn run_certify_shard(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let scenario = flags.get("scenario").expect("--scenario is required");
    let index: usize = flags
        .get("shard")
        .expect("--shard is required")
        .parse()
        .expect("invalid shard");
    let count: usize = flags
        .get("of")
        .expect("--of is required")
        .parse()
        .expect("invalid shard count");
    let out = PathBuf::from(flags.get("out").expect("--out is required"));
    let shard = gisard::solvers::Shard::new(index, count);
    let options = shard_options(&flags);

    let (manifest, instance) = shard_instance(dir);
    let started = std::time::Instant::now();
    let windows =
        gisard::campaign::certify_shard(dir, &instance, &manifest, scenario, shard, &options, &out)
            .expect("shard write failed");
    println!(
        "{scenario} shard {index}/{count}: {windows} windows in {:.0}s -> {}",
        started.elapsed().as_secs_f64(),
        out.display(),
    );
}

fn run_polish_merge(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let shards = PathBuf::from(flags.get("shards").expect("--shards is required"));
    let (manifest, instance) = shard_instance(dir);

    let scenarios: Vec<String> = match flags.get("scenario") {
        Some(id) => vec![id.clone()],
        None => manifest.scenarios.iter().map(|s| s.id.clone()).collect(),
    };

    let mut total = 0i64;
    for scenario in &scenarios {
        let report =
            match gisard::campaign::merge_shards(dir, &instance, &manifest, scenario, &shards) {
                Ok(report) => report,
                Err(e) => {
                    println!("{scenario:<14} skipped: {e}");
                    continue;
                }
            };
        total += report.after as i64 - report.before as i64;
        println!(
            "{:<14} {} -> {}  ({:+})  {} shards, {} windows, {} proved ({:.0}%), \
             {} improving, {} applied{}",
            report.scenario,
            report.before,
            report.after,
            report.after as i64 - report.before as i64,
            report.shards,
            report.windows,
            report.proved,
            100.0 * report.proved as f64 / report.windows.max(1) as f64,
            report.improving,
            report.applied,
            if report.kept { "  kept" } else { "" },
        );
    }
    println!("total gained: {total}");
}

fn run_polish(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let manifest = Manifest::load(&dir.join("manifest.json"));
    let dataset = manifest.dataset_path(dir);
    let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);

    let mut options = PolishOptions::default();
    if let Some(p) = Probe::load(&dir.join("probe.json")) {
        options.spacing = p.recommended_spacing;
    }
    if let Some(v) = flags.get("radius") {
        options.radius = v.parse().expect("invalid radius");
    }
    if let Some(v) = flags.get("windows") {
        options.windows = Some(v.parse().expect("invalid windows"));
    }
    if let Some(v) = flags.get("rounds") {
        options.rounds = v.parse().expect("invalid rounds");
    }

    let projected: usize = manifest
        .scenarios
        .iter()
        .map(|s| {
            options
                .windows
                .unwrap_or(s.k.min(gisard::campaign::MAX_WINDOWS))
        })
        .sum();
    println!(
        "window radius {}, {} round(s), 2 centrings, ~{} windows, roughly {:.0} min",
        options.radius,
        options.rounds,
        2 * projected,
        2.0 * projected as f64 * 1.1 * options.rounds as f64 / 60.0,
    );
    let results = polish(dir, &instance, &manifest, &options);
    for r in &results {
        println!(
            "{:<14} {} -> {}  ({:+}) in {:.0}s{}",
            r.scenario,
            r.before,
            r.after,
            r.after as i64 - r.before as i64,
            r.seconds,
            if r.kept { "  kept" } else { "" },
        );
    }
    let gained: i64 = results
        .iter()
        .filter(|r| r.kept)
        .map(|r| r.after as i64 - r.before as i64)
        .sum();
    println!("total gained: {gained}");
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StatusCache {
    version: u32,
    seen: HashSet<String>,
    timed: Vec<TimedResult>,
}

fn cached_timed_results(queue: &Queue, path: &Path) -> Vec<TimedResult> {
    let mut cache = std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<StatusCache>(&body).ok())
        .filter(|cache| cache.version == 1)
        .unwrap_or(StatusCache {
            version: 1,
            seen: HashSet::new(),
            timed: Vec::new(),
        });
    cache
        .timed
        .extend(queue.timed_results_since(&mut cache.seen));
    cache
        .timed
        .sort_unstable_by_key(|timed| timed.completed_unix);

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    if let Ok(body) = serde_json::to_vec(&cache)
        && std::fs::write(&temporary, body).is_ok()
    {
        let _ = std::fs::rename(&temporary, path);
    }
    cache.timed
}

fn status(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let compact = flags.contains_key("compact");
    let mut manifest = Manifest::load(&dir.join("manifest.json"));
    let queue = Queue::new(dir);
    let timed = flags
        .get("cache")
        .filter(|path| !path.is_empty())
        .map_or_else(
            || queue.timed_results(),
            |path| cached_timed_results(&queue, Path::new(path)),
        );
    let claims = queue.running_claims();
    let now = now_unix();
    let events = read_events(dir);
    let clock = manifest.clock(dir);
    if let Some(clock) = clock {
        manifest.start_unix = clock.start_unix;
        manifest.deadline_unix = clock.deadline_unix;
    }
    let elapsed = if manifest.start_unix > 0 {
        now.saturating_sub(manifest.start_unix)
    } else {
        0
    };
    let stop_at = manifest
        .deadline_unix
        .saturating_sub(manifest.reserve_seconds);
    let remaining = stop_at.saturating_sub(now);
    let mode = if manifest.duration_seconds > 0 {
        format!(
            "relative {:.2}h from first worker",
            manifest.duration_seconds as f64 / 3600.0
        )
    } else if manifest.deadline_unix > 0 {
        format!("fixed Unix {}", manifest.deadline_unix)
    } else {
        "no deadline".to_string()
    };

    println!("campaign {}", dir.display());
    println!("snapshot   Unix {now}");
    if manifest.duration_seconds > 0 && clock.is_none() {
        println!("clock      {mode}; WAITING FOR FIRST WORKER");
    } else {
        println!(
            "clock      {mode}; started Unix {}; elapsed {}; work stops in {} ({} reserve)",
            manifest.start_unix,
            human_duration(elapsed),
            if manifest.deadline_unix > 0 {
                human_duration(remaining)
            } else {
                "unbounded".into()
            },
            human_duration(manifest.reserve_seconds),
        );
    }

    let completed = timed.len();
    let promotions = timed
        .iter()
        .filter(|timed| timed.result.trial.rung > 0)
        .count();
    let recent = timed
        .iter()
        .filter(|timed| timed.completed_unix >= now.saturating_sub(3600))
        .count();
    let recent_five = timed
        .iter()
        .filter(|timed| timed.completed_unix >= now.saturating_sub(300))
        .count();
    let trial_wall: f64 = timed.iter().map(|timed| timed.result.seconds).sum();
    let workers: std::collections::HashSet<&str> = timed
        .iter()
        .map(|timed| timed.result.worker.as_str())
        .chain(claims.iter().map(|claim| claim.worker.as_str()))
        .filter(|worker| !worker.is_empty())
        .collect();
    let active_slots = workers.len().max(claims.len());
    let busy = if active_slots > 0 && elapsed > 0 {
        100.0 * trial_wall / (active_slots as f64 * elapsed as f64)
    } else {
        0.0
    };
    let recent_window = elapsed.clamp(1, 3600);
    let recent_rate = recent as f64 * 3600.0 / recent_window as f64;
    println!(
        "work       {completed} completed, {} running, {promotions} cohort promotions, {recent_five} in 5m, {recent} in 1h",
        claims.len(),
    );
    println!(
        "throughput {:.1} trials/hour overall, {recent_rate:.1}/hour recently; {} worker ids, {active_slots} active slots; {:.0}% completed-trial utilisation",
        if elapsed > 0 {
            completed as f64 * 3600.0 / elapsed as f64
        } else {
            0.0
        },
        workers.len(),
        busy,
    );
    let latest_event = events.last().map(|event| event.unix_ms / 1000);
    let checkpoints_hour = events
        .iter()
        .filter(|event| {
            event.kind == "checkpoint" && event.unix_ms / 1000 >= now.saturating_sub(3600)
        })
        .count();
    println!(
        "events     {} recorded, {} score checkpoints in 1h, newest {}",
        events.len(),
        checkpoints_hour,
        latest_event.map_or("none".to_string(), |time| format!(
            "{} ago",
            human_duration(now.saturating_sub(time))
        )),
    );
    let claiming_workers: std::collections::HashSet<&str> =
        claims.iter().map(|claim| claim.worker.as_str()).collect();
    if claiming_workers.len() < claims.len() {
        println!(
            "ALERT      {} active claims share only {} worker ids; worker ids must be unique",
            claims.len(),
            claiming_workers.len(),
        );
    }
    let failed_events = events
        .iter()
        .filter(|event| event.kind.ends_with("_failed"))
        .count();
    if failed_events > 0 {
        println!(
            "ALERT      {failed_events} failed lifecycle events recorded; inspect `campaign events`"
        );
    }

    println!("\nscenarios");
    for scenario in &manifest.scenarios {
        let mine: Vec<_> = timed
            .iter()
            .filter(|timed| timed.result.trial.scenario == scenario.id)
            .collect();
        let running = claims
            .iter()
            .filter(|claim| {
                claim
                    .trial
                    .as_ref()
                    .is_some_and(|trial| trial.scenario == scenario.id)
            })
            .count();
        let first = mine.first().map(|timed| timed.result.serviced);
        let best_result = mine.iter().map(|timed| timed.result.serviced).max();
        let best_update = queue.best_score_update(&scenario.id);
        let best = best_update.map(|(score, _)| score).or(best_result);
        let gain = best
            .zip(first)
            .map_or(0, |(best, first)| best.saturating_sub(first));
        let before_recent = mine
            .iter()
            .filter(|timed| timed.completed_unix < now.saturating_sub(3600))
            .map(|timed| timed.result.serviced)
            .max()
            .or(first)
            .unwrap_or(0);
        let recent_completed_best = mine
            .iter()
            .filter(|timed| timed.completed_unix >= now.saturating_sub(3600))
            .map(|timed| timed.result.serviced)
            .max()
            .unwrap_or(before_recent);
        let recent_best =
            if best_update.is_some_and(|(_, updated)| updated >= now.saturating_sub(3600)) {
                best.unwrap_or(recent_completed_best)
            } else {
                recent_completed_best
            };
        let recent_gain = recent_best.saturating_sub(before_recent);
        let completed_hour = mine
            .iter()
            .filter(|timed| timed.completed_unix >= now.saturating_sub(3600))
            .count();
        let seconds: f64 = mine.iter().map(|timed| timed.result.seconds).sum();
        let allocation = if trial_wall > 0.0 {
            100.0 * seconds / trial_wall
        } else {
            0.0
        };
        let last = mine.last().map_or("never".to_string(), |timed| {
            format!(
                "{} ago",
                human_duration(now.saturating_sub(timed.completed_unix))
            )
        });
        let last_score = events
            .iter()
            .rev()
            .find(|event| {
                event.scenario.as_deref() == Some(&scenario.id) && event.kind == "checkpoint"
            })
            .map_or_else(
                || {
                    best_update.map_or_else(
                        || last.clone(),
                        |(_, updated)| {
                            format!("{} ago", human_duration(now.saturating_sub(updated)))
                        },
                    )
                },
                |event| {
                    format!(
                        "{} ago",
                        human_duration(now.saturating_sub(event.unix_ms / 1000))
                    )
                },
            );
        println!(
            "  {:<12} best {:<8} gain +{:<6} 1h +{:<5} done {:<5} 1h {:<4} run {:<4} saved {:<2} alloc {:>5.1}% score {}",
            scenario.id,
            best.map_or("-".to_string(), |score| score.to_string()),
            gain,
            recent_gain,
            mine.len(),
            completed_hour,
            running,
            queue.candidate_count(&scenario.id),
            allocation,
            last_score,
        );

        if compact {
            continue;
        }
        let mut rungs: BTreeMap<(u64, usize), Vec<_>> = BTreeMap::new();
        for timed in &mine {
            rungs
                .entry((
                    timed.result.trial.config.spacing.to_bits(),
                    timed.result.trial.rung,
                ))
                .or_default()
                .push(*timed);
        }
        for ((spacing_bits, rung), entries) in rungs {
            let spacing = f64::from_bits(spacing_bits);
            let budget = entries
                .iter()
                .map(|timed| timed.result.trial.iterations)
                .max()
                .unwrap_or(0);
            let average = entries
                .iter()
                .map(|timed| timed.result.seconds)
                .sum::<f64>()
                / entries.len() as f64;
            let rung_best = entries
                .iter()
                .map(|timed| timed.result.serviced)
                .max()
                .unwrap_or(0);
            println!(
                "    spacing {:<10.5} rung {rung}: {:<5} complete, budget {:<6}, best {:<8}, mean wall {}",
                spacing,
                entries.len(),
                budget,
                rung_best,
                human_duration(average.round() as u64),
            );
        }
    }

    if !claims.is_empty() && !compact {
        let oldest = claims
            .iter()
            .map(|claim| claim.age_seconds)
            .max()
            .unwrap_or(0);
        println!(
            "\nrunning    {} claims; oldest has run {}",
            claims.len(),
            human_duration(oldest)
        );
        let mut groups: BTreeMap<(String, u64, usize), usize> = BTreeMap::new();
        for claim in &claims {
            let key = claim.trial.as_ref().map_or_else(
                || ("unknown".into(), 0, 0),
                |trial| {
                    (
                        trial.scenario.clone(),
                        trial.config.spacing.to_bits(),
                        trial.rung,
                    )
                },
            );
            *groups.entry(key).or_default() += 1;
        }
        for ((scenario, spacing_bits, rung), count) in groups {
            println!(
                "  {scenario} spacing {:.5} rung {rung}: {count}",
                f64::from_bits(spacing_bits)
            );
        }
        if flags.contains_key("claims") {
            println!("\nclaim details");
            for claim in &claims {
                println!(
                    "  {} worker {:<20} age {}",
                    claim.trial_id,
                    claim.worker,
                    human_duration(claim.age_seconds)
                );
            }
        }
    }

    let unscored: Vec<&str> = manifest
        .scenarios
        .iter()
        .filter(|s| queue.best_score(&s.id).is_none())
        .map(|s| s.id.as_str())
        .collect();
    if !unscored.is_empty() {
        println!(
            "NOT YET SUBMITTABLE — no solution for: {}",
            unscored.join(", ")
        );
    }
}

fn events(dir: &Path, rest: &[String]) {
    let flags = flags(rest);
    let limit = flags
        .get("limit")
        .map_or(20, |value| value.parse().expect("invalid event limit"));
    let since = flags
        .get("since-seconds")
        .map(|value| value.parse::<u64>().expect("invalid event age"));
    let scenario = flags.get("scenario").filter(|value| !value.is_empty());
    let now_ms = now_unix() * 1000;
    let mut events: Vec<CampaignEvent> = read_events(dir)
        .into_iter()
        .filter(|event| {
            since.is_none_or(|seconds| event.unix_ms >= now_ms.saturating_sub(seconds * 1000))
                && scenario.is_none_or(|wanted| event.scenario.as_ref() == Some(wanted))
        })
        .collect();
    if events.is_empty() {
        if !flags.contains_key("json") {
            println!("(no worker events recorded yet)");
        }
        return;
    }
    if limit > 0 && events.len() > limit {
        events.drain(..events.len() - limit);
    }
    for event in events {
        if flags.contains_key("json") {
            println!(
                "{}",
                serde_json::to_string(&event).expect("event is serializable")
            );
            continue;
        }
        let age = human_duration(now_ms.saturating_sub(event.unix_ms) / 1000);
        let subject = event.scenario.as_deref().unwrap_or("-");
        let score = event
            .score
            .map_or(String::new(), |score| format!(" score={score}"));
        let trial = event
            .trial
            .as_deref()
            .map_or(String::new(), |trial| format!(" trial={trial}"));
        let rung = event.rung.map_or(String::new(), |rung| format!(" r{rung}"));
        let detail = event
            .detail
            .as_deref()
            .map_or(String::new(), |detail| format!(" {detail}"));
        println!(
            "{:>8}  {:<20} {:<12} {:<22}{}{}{}{}",
            format!("{age} ago"),
            event.kind,
            subject,
            event.worker,
            rung,
            score,
            trial,
            detail,
        );
    }
}

fn human_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn harvest_cmd(dir: &Path, rest: &[String]) -> ExitCode {
    let flags = flags(rest);
    let manifest = Manifest::load(&dir.join("manifest.json"));
    let dataset = manifest.dataset_path(dir);
    let instance = parse_instance(dataset.to_str().expect("non-utf8 dataset path"), 1, 0.5);

    match harvest(dir, &instance, Options::default()) {
        Ok(submission) => {
            let out = flags.get("out").map_or_else(
                || dir.join("submission").join("solution.txt"),
                PathBuf::from,
            );
            for s in &submission.scenarios {
                print!("{:<12} claimed {:<6}", s.id, s.serviced.len());
                let f = s.fixes;
                if !f.is_clean() {
                    print!(
                        "  [repaired {} padded {} trimmed {} withheld {}]",
                        f.repaired, f.padded, f.trimmed, f.claims_withheld
                    );
                }
                if f.repairs_lossy > 0 {
                    print!("  WARNING: {} repairs cost claims", f.repairs_lossy);
                }
                println!();
            }
            submission.write(&out);
            println!("wrote {}", out.display());
            ExitCode::SUCCESS
        }
        Err(violations) => {
            eprintln!("{} gate violations, nothing written:", violations.len());
            for v in &violations {
                eprintln!("  {v}");
            }
            ExitCode::FAILURE
        }
    }
}
