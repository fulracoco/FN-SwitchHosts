use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

pub const MAX_CONTENT: usize = 4 * 1024 * 1024;
pub const MAX_TOTAL: usize = 16 * 1024 * 1024;
pub const HISTORY_LIMIT: usize = 100;
pub const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub interval_seconds: u64,
    #[serde(default)]
    pub last_attempt: i64,
    #[serde(default)]
    pub last_success: i64,
    #[serde(default)]
    pub last_error: String,
}

impl Entry {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid entry ID"
        );
        ensure!(
            !self.title.trim().is_empty() && self.title.len() <= 200,
            "title must contain 1–200 bytes"
        );
        ensure!(self.content.len() <= MAX_CONTENT, "entry exceeds 4 MiB");
        crate::hosts_text::validate_system_text(&self.content)?;
        ensure!(
            self.interval_seconds == 0 || (60..=2_592_000).contains(&self.interval_seconds),
            "refresh interval must be 0 or 60–2592000 seconds"
        );
        ensure!(self.url.len() <= 4096, "subscription URL is too long");
        if !self.url.is_empty() {
            let url = reqwest::Url::parse(&self.url)?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none(),
                "subscription requires an HTTP(S) URL without embedded credentials"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub id: String,
    pub created: i64,
    pub label: String,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub schema: u32,
    pub revision: u64,
    pub entries: Vec<Entry>,
    pub history: Vec<Snapshot>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema: 1,
            revision: 0,
            entries: vec![],
            history: vec![],
        }
    }
}

impl Document {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported data schema");
        ensure!(
            self.entries.len() <= 500,
            "at most 500 entries are supported"
        );
        let mut ids = std::collections::HashSet::new();
        let mut total = 0;
        for entry in &self.entries {
            entry.validate()?;
            ensure!(ids.insert(&entry.id), "duplicate entry ID");
            total += entry.content.len();
        }
        ensure!(total <= MAX_TOTAL, "combined entries exceed 16 MiB");
        ensure!(
            self.history.len() <= HISTORY_LIMIT,
            "too many history snapshots"
        );
        ensure!(
            self.history.iter().map(|s| s.content.len()).sum::<usize>() <= MAX_HISTORY_BYTES,
            "history exceeds 64 MiB"
        );
        for snapshot in &self.history {
            ensure!(
                snapshot.content.len() <= MAX_TOTAL,
                "history snapshot is too large"
            );
            crate::hosts_text::validate_system_text(&snapshot.content)?;
        }
        Ok(())
    }

    pub fn check_revision(&self, revision: u64) -> Result<()> {
        if self.revision != revision {
            bail!("conflict: data changed; reload before saving");
        }
        Ok(())
    }

    pub fn snapshot(&mut self, label: &str, content: &str) {
        if self
            .history
            .last()
            .is_some_and(|item| item.content == content)
        {
            return;
        }
        self.history.push(Snapshot {
            id: uuid::Uuid::new_v4().to_string(),
            created: chrono::Utc::now().timestamp(),
            label: label.into(),
            content: content.into(),
        });
        let remove = self.history.len().saturating_sub(HISTORY_LIMIT);
        self.history.drain(..remove);
        while self.history.iter().map(|s| s.content.len()).sum::<usize>() > MAX_HISTORY_BYTES {
            self.history.remove(0);
        }
    }
}
