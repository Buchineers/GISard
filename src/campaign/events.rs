use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CampaignEvent {
    pub unix_ms: u64,
    pub kind: String,
    pub worker: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rung: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spacing: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl CampaignEvent {
    pub fn worker(kind: &str, worker: &str, detail: String) -> Self {
        CampaignEvent {
            unix_ms: now_ms(),
            kind: kind.to_string(),
            worker: worker.to_string(),
            trial: None,
            scenario: None,
            score: None,
            rung: None,
            spacing: None,
            detail: Some(detail),
        }
    }

    pub fn trial(kind: &str, worker: &str, trial: &super::Trial, score: Option<usize>) -> Self {
        CampaignEvent {
            unix_ms: now_ms(),
            kind: kind.to_string(),
            worker: worker.to_string(),
            trial: Some(trial.id.clone()),
            scenario: Some(trial.scenario.clone()),
            score,
            rung: Some(trial.rung),
            spacing: Some(trial.config.spacing),
            detail: None,
        }
    }
}

pub struct EventLog {
    file: Mutex<File>,
}

impl EventLog {
    pub fn open(root: &Path, worker: &str) -> io::Result<Self> {
        let dir = root.join("events/workers");
        fs::create_dir_all(&dir)?;
        let worker_name: String = worker
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let process_name = match (
            std::env::var("SLURM_JOB_ID"),
            std::env::var("SLURM_ARRAY_TASK_ID"),
            std::env::var("SLURM_PROCID"),
        ) {
            (Ok(job), Ok(task), Ok(process)) => format!("{job}-{task}-{process}"),
            _ => std::process::id().to_string(),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{worker_name}--{process_name}.jsonl")))?;
        Ok(EventLog {
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, event: &CampaignEvent) {
        let Ok(mut file) = self.file.lock() else {
            eprintln!("campaign event log lock is poisoned");
            return;
        };
        let Ok(mut line) = serde_json::to_vec(event) else {
            eprintln!("campaign event could not be serialized");
            return;
        };
        line.push(b'\n');
        if let Err(error) = file.write_all(&line).and_then(|()| file.flush()) {
            eprintln!("campaign event log write failed: {error}");
        }
    }
}

pub fn read_events(root: &Path) -> Vec<CampaignEvent> {
    let mut events = Vec::new();
    for entry in fs::read_dir(root.join("events/workers"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let Ok(file) = File::open(entry.path()) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(event) = serde_json::from_str(&line) {
                events.push(event);
            }
        }
    }
    events.sort_unstable_by_key(|event: &CampaignEvent| event.unix_ms);
    events
}

#[derive(Default)]
pub struct EventReader {
    offsets: HashMap<PathBuf, u64>,
}

impl EventReader {
    pub fn follow_from_end(&mut self, root: &Path) {
        for entry in fs::read_dir(root.join("events/workers"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if let Ok(length) = entry.metadata().map(|metadata| metadata.len()) {
                self.offsets.insert(path, length);
            }
        }
    }

    pub fn read_new(&mut self, root: &Path) -> Vec<CampaignEvent> {
        let mut events = Vec::new();
        for entry in fs::read_dir(root.join("events/workers"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let offset = self.offsets.entry(path.clone()).or_default();
            let Ok(length) = entry.metadata().map(|metadata| metadata.len()) else {
                continue;
            };
            if length < *offset {
                *offset = 0;
            }
            if length == *offset {
                continue;
            }
            let Ok(mut file) = File::open(&path) else {
                continue;
            };
            if file.seek(SeekFrom::Start(*offset)).is_err() {
                continue;
            }
            let unread = length.saturating_sub(*offset);
            let mut reader = BufReader::new(file.take(unread));
            let mut line = String::new();
            loop {
                line.clear();
                let Ok(read) = reader.read_line(&mut line) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                *offset += read as u64;
                if let Ok(event) = serde_json::from_str(line.trim_end()) {
                    events.push(event);
                }
            }
        }
        events.sort_unstable_by_key(|event: &CampaignEvent| event.unix_ms);
        events
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_logs_are_append_only_and_merge_chronologically() {
        let root = std::env::temp_dir().join(format!("gisard-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let first = EventLog::open(&root, "a/1").unwrap();
        let second = EventLog::open(&root, "b").unwrap();
        first.record(&CampaignEvent::worker(
            "worker_start",
            "a/1",
            "first".into(),
        ));
        second.record(&CampaignEvent::worker("worker_start", "b", "second".into()));
        first.record(&CampaignEvent::worker("worker_stop", "a/1", "done".into()));

        let events = read_events(&root);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().filter(|event| event.worker == "a/1").count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "worker_stop")
                .count(),
            1
        );
        assert_eq!(
            fs::read_dir(root.join("events/workers")).unwrap().count(),
            2
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_reader_returns_each_event_once() {
        let root = std::env::temp_dir().join(format!("gisard-event-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let log = EventLog::open(&root, "worker").unwrap();
        let mut reader = EventReader::default();

        log.record(&CampaignEvent::worker(
            "worker_start",
            "worker",
            "one".into(),
        ));
        assert_eq!(reader.read_new(&root).len(), 1);
        assert!(reader.read_new(&root).is_empty());
        log.record(&CampaignEvent::worker(
            "worker_stop",
            "worker",
            "two".into(),
        ));
        assert_eq!(reader.read_new(&root).len(), 1);
        assert!(reader.read_new(&root).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn follower_starts_after_existing_history() {
        let root = std::env::temp_dir().join(format!("gisard-event-follow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let log = EventLog::open(&root, "worker").unwrap();
        log.record(&CampaignEvent::worker(
            "worker_start",
            "worker",
            "old".into(),
        ));

        let mut reader = EventReader::default();
        reader.follow_from_end(&root);
        assert!(reader.read_new(&root).is_empty());
        log.record(&CampaignEvent::worker(
            "worker_stop",
            "worker",
            "new".into(),
        ));
        assert_eq!(reader.read_new(&root).len(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
