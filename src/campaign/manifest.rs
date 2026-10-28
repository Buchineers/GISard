use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub k: usize,
    pub tau: f64,
}

impl Scenario {
    pub fn new(tau: f64, k: usize) -> Self {
        Scenario {
            id: format!("t{:03}-k{k}", (tau * 100.0).round() as i64),
            k,
            tau,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub dataset: PathBuf,
    pub scenarios: Vec<Scenario>,
    #[serde(default)]
    pub start_unix: u64,
    #[serde(default)]
    pub deadline_unix: u64,
    #[serde(default)]
    pub reserve_seconds: u64,
    #[serde(default)]
    pub duration_seconds: u64,
    #[serde(default)]
    pub spacings: Vec<f64>,
    #[serde(default)]
    pub buildings: usize,
}

impl Manifest {
    pub fn is_solved(&self, serviced: usize) -> bool {
        self.buildings > 0 && serviced >= self.buildings
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CampaignClock {
    pub start_unix: u64,
    pub deadline_unix: u64,
}

const CLOCK_FILE: &str = "campaign-clock.json";

impl Manifest {
    pub fn load(path: &Path) -> Self {
        let data = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let manifest: Manifest = serde_json::from_str(&data)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
        assert!(!manifest.scenarios.is_empty(), "manifest has no scenarios");
        assert!(
            manifest.duration_seconds == 0 || manifest.deadline_unix == 0,
            "manifest mixes a relative duration with an absolute deadline"
        );
        for s in &manifest.scenarios {
            assert!(s.k > 0, "scenario {}: k must be positive", s.id);
            assert!(
                s.tau > 0.0 && s.tau <= 1.0,
                "scenario {}: tau must be in (0, 1]",
                s.id
            );
        }
        manifest
    }

    pub fn save(&self, path: &Path) {
        let data = serde_json::to_string_pretty(self).expect("failed to encode manifest");
        fs::write(path, data).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }

    pub fn dataset_path(&self, campaign: &Path) -> PathBuf {
        if self.dataset.is_absolute() {
            self.dataset.clone()
        } else {
            campaign.join(&self.dataset)
        }
    }

    pub fn reset_clock(campaign: &Path) {
        match fs::remove_file(campaign.join(CLOCK_FILE)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("failed to reset the campaign clock: {error}"),
        }
    }

    pub fn clock(&self, campaign: &Path) -> Option<CampaignClock> {
        if self.duration_seconds == 0 {
            return Some(CampaignClock {
                start_unix: self.start_unix,
                deadline_unix: self.deadline_unix,
            });
        }
        read_clock(&campaign.join(CLOCK_FILE))
    }

    pub fn activate_clock(&mut self, campaign: &Path) -> CampaignClock {
        if let Some(clock) = self.clock(campaign) {
            self.start_unix = clock.start_unix;
            self.deadline_unix = clock.deadline_unix;
            return clock;
        }

        let now = now_unix();
        let proposed = CampaignClock {
            start_unix: now,
            deadline_unix: now.saturating_add(self.duration_seconds),
        };
        let path = campaign.join(CLOCK_FILE);
        let body = serde_json::to_vec(&proposed).expect("campaign clock is serializable");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(&body)
                    .unwrap_or_else(|error| panic!("failed to start campaign clock: {error}"));
                file.sync_data().ok();
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("failed to create {}: {error}", path.display()),
        }

        for _ in 0..1_000 {
            if let Some(clock) = read_clock(&path) {
                self.start_unix = clock.start_unix;
                self.deadline_unix = clock.deadline_unix;
                return clock;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("campaign clock was created but never became readable");
    }
}

fn read_clock(path: &Path) -> Option<CampaignClock> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn relative() -> (PathBuf, Manifest) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "gisard-clock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let manifest = Manifest {
            dataset: "d.geojson".into(),
            scenarios: vec![Scenario::new(0.5, 1)],
            start_unix: 0,
            deadline_unix: 0,
            reserve_seconds: 0,
            duration_seconds: 7_200,
            spacings: vec![8.0],
            buildings: 0,
        };
        (root, manifest)
    }

    #[test]
    fn reading_status_does_not_start_a_relative_clock() {
        let (root, manifest) = relative();
        assert_eq!(manifest.clock(&root), None);
        assert!(!root.join(CLOCK_FILE).exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_absolute_deadline_needs_no_shared_clock_file() {
        let (root, mut manifest) = relative();
        manifest.duration_seconds = 0;
        manifest.start_unix = 100;
        manifest.deadline_unix = 200;
        assert_eq!(
            manifest.activate_clock(&root),
            CampaignClock {
                start_unix: 100,
                deadline_unix: 200,
            }
        );
        assert!(!root.join(CLOCK_FILE).exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn every_worker_observes_the_first_process_clock() {
        let (root, manifest) = relative();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (root, barrier, mut manifest) =
                    (root.clone(), barrier.clone(), manifest.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    manifest.activate_clock(&root)
                })
            })
            .collect();
        let clocks: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(clocks.iter().all(|clock| *clock == clocks[0]));
        assert_eq!(clocks[0].deadline_unix - clocks[0].start_unix, 7_200);
        fs::remove_dir_all(root).ok();
    }
}
