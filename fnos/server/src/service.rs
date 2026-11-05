use crate::{
    model::{Document, Entry, MAX_CONTENT, MAX_TOTAL},
    target::Target,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

#[derive(Deserialize, Serialize)]
struct Pending {
    before: String,
    after: String,
    next: Document,
}

pub struct Service {
    pub(crate) data: Mutex<Document>,
    directory: PathBuf,
    pub target: Target,
    client: reqwest::Client,
    _instance_lock: std::fs::File,
}

// Flush the journal before touching hosts. Replacement is atomic on the target
// Linux filesystem; parent fsync makes the rename durable across power loss.
fn durable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    #[cfg(unix)]
    std::fs::File::open(path.parent().context("missing data directory")?)?.sync_all()?;
    Ok(())
}

impl Service {
    pub async fn open(directory: PathBuf, target: Target) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&directory)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("instance.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("another FN-SwitchHosts service is using this directory")?;
        let path = directory.join("state.json");
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Document>(&bytes)
                .context("state.json is corrupt; original file retained")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Document::default(),
            Err(e) => return Err(e.into()),
        };
        data.validate()?;
        let service = Arc::new(Self {
            data: Mutex::new(data),
            directory,
            target,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("FN-SwitchHosts/0.1")
                .build()?,
            _instance_lock: lock,
        });
        // A differing external file requires explicit acknowledgement in the UI.
        if let Err(error) = service.reconcile().await {
            eprintln!("recovery pending: {error:#}");
        }
        Ok(service)
    }

    fn pending_path(&self) -> PathBuf {
        self.directory.join("pending.json")
    }
    pub fn recovery_required(&self) -> bool {
        self.pending_path().exists()
    }

    fn save(&self, document: &Document) -> Result<()> {
        document.validate()?;
        durable_json(&self.directory.join("state.json"), document)
    }

    fn clear_pending(&self) -> Result<()> {
        std::fs::remove_file(self.pending_path())?;
        #[cfg(unix)]
        std::fs::File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    fn ready(&self) -> Result<()> {
        ensure!(
            !self.recovery_required(),
            "recovery: an interrupted apply must be reconciled first"
        );
        Ok(())
    }

    pub async fn document(&self) -> Document {
        self.data.lock().await.clone()
    }

    pub async fn reconcile(&self) -> Result<()> {
        let mut guard = self.data.lock().await;
        if !self.recovery_required() {
            return Ok(());
        }
        let pending: Pending = serde_json::from_slice(&std::fs::read(self.pending_path())?)?;
        pending.next.validate()?;
        let current = self.target.read().await?;
        if current == pending.after {
            self.save(&pending.next)?;
            *guard = pending.next;
        } else if current != pending.before {
            bail!("recovery: hosts changed externally; keep the current file to acknowledge and resume");
        }
        self.clear_pending()
    }

    pub async fn keep_current(&self, revision: u64) -> Result<()> {
        let mut guard = self.data.lock().await;
        guard.check_revision(revision)?;
        ensure!(self.recovery_required(), "no recovery is pending");
        let mut next = guard.clone();
        for entry in &mut next.entries {
            entry.enabled = false;
        }
        next.snapshot("保留外部 hosts，暂停全部方案", &self.target.read().await?);
        next.revision += 1;
        self.save(&next)?;
        *guard = next;
        self.clear_pending()
    }

    // The data mutex covers selection, write, history, and persistence as one
    // transaction. A durable pending record repairs crashes at either commit.
    async fn apply(
        &self,
        guard: &mut Document,
        mut next: Document,
        restored: Option<String>,
    ) -> Result<()> {
        self.ready()?;
        let before = self.target.read().await?;
        let after = restored.unwrap_or_else(|| {
            let content = next
                .entries
                .iter()
                .filter(|e| e.enabled)
                .map(|e| crate::hosts_text::normalize(&e.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            crate::managed_content::make_append_content(
                &crate::hosts_text::normalize(&before),
                &content,
            )
        });
        ensure!(after.len() <= MAX_TOTAL, "combined hosts exceeds 16 MiB");
        next.snapshot("写入前", &before);
        next.snapshot("已应用", &after);
        next.revision += 1;
        next.validate()?;
        durable_json(
            &self.pending_path(),
            &Pending {
                before: before.clone(),
                after: after.clone(),
                next: next.clone(),
            },
        )?;
        if let Err(error) = self.target.write(&before, &after).await {
            // Even a lost response may mean a successful write. Keep the
            // journal until its actual outcome has been reconciled.
            return Err(error.context("apply incomplete; reconcile before further changes"));
        }
        self.save(&next)?;
        *guard = next;
        self.clear_pending()
    }

    pub async fn save_entry(&self, revision: u64, mut entry: Entry) -> Result<()> {
        entry.validate()?;
        let mut guard = self.data.lock().await;
        self.ready()?;
        guard.check_revision(revision)?;
        let mut next = guard.clone();
        let mut applies = entry.enabled;
        if let Some(previous) = next.entries.iter_mut().find(|e| e.id == entry.id) {
            applies |= previous.enabled;
            // Subscription status is backend-owned; editing a URL resets it.
            if previous.url == entry.url {
                entry.last_attempt = previous.last_attempt;
                entry.last_success = previous.last_success;
                entry.last_error = previous.last_error.clone();
            } else {
                entry.last_attempt = 0;
                entry.last_success = 0;
                entry.last_error.clear();
            }
            *previous = entry;
        } else {
            entry.last_attempt = 0;
            entry.last_success = 0;
            entry.last_error.clear();
            next.entries.push(entry);
        }
        next.validate()?;
        if applies {
            self.apply(&mut guard, next, None).await
        } else {
            next.revision += 1;
            self.save(&next)?;
            *guard = next;
            Ok(())
        }
    }

    pub async fn delete(&self, revision: u64, id: &str) -> Result<()> {
        let mut guard = self.data.lock().await;
        self.ready()?;
        guard.check_revision(revision)?;
        let entry = guard
            .entries
            .iter()
            .find(|e| e.id == id)
            .context("entry not found")?;
        let applies = entry.enabled;
        let mut next = guard.clone();
        next.entries.retain(|e| e.id != id);
        if applies {
            self.apply(&mut guard, next, None).await
        } else {
            next.revision += 1;
            self.save(&next)?;
            *guard = next;
            Ok(())
        }
    }

    pub async fn restore(&self, revision: u64, id: &str) -> Result<()> {
        let mut guard = self.data.lock().await;
        guard.check_revision(revision)?;
        let snapshot = guard
            .history
            .iter()
            .find(|s| s.id == id)
            .context("history not found")?
            .content
            .clone();
        let mut next = guard.clone();
        for entry in &mut next.entries {
            entry.enabled = false;
        }
        self.apply(&mut guard, next, Some(snapshot)).await
    }

    async fn fetch(&self, url: &str) -> Result<String> {
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_CONTENT as u64,
            "subscription exceeds 4 MiB"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_CONTENT,
                "subscription exceeds 4 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        let content = String::from_utf8(bytes).context("subscription must be UTF-8")?;
        crate::hosts_text::validate_system_text(&content)?;
        Ok(crate::hosts_text::normalize(&content))
    }

    pub async fn refresh(&self, id: &str) -> Result<()> {
        self.ready()?;
        let original = self
            .data
            .lock()
            .await
            .entries
            .iter()
            .find(|e| e.id == id)
            .context("entry not found")?
            .clone();
        ensure!(!original.url.is_empty(), "entry is not a subscription");
        let result = self.fetch(&original.url).await;
        let mut guard = self.data.lock().await;
        self.ready()?;
        let mut next = guard.clone();
        let entry = next
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .context("conflict: entry deleted while downloading")?;
        ensure!(
            entry.url == original.url
                && entry.content == original.content
                && entry.last_attempt == original.last_attempt,
            "conflict: subscription changed while downloading"
        );
        entry.last_attempt = chrono::Utc::now().timestamp();
        let mut changed = false;
        match &result {
            Ok(content) => {
                changed = *content != entry.content;
                entry.content = content.clone();
                entry.last_success = entry.last_attempt;
                entry.last_error.clear();
            }
            // Avoid leaking signed subscription URLs through network errors.
            Err(_) => entry.last_error = "订阅获取失败，请检查地址、网络及远端响应".into(),
        }
        let applies = changed && entry.enabled;
        if applies {
            self.apply(&mut guard, next, None).await?;
        } else {
            next.revision += 1;
            self.save(&next)?;
            *guard = next;
        }
        result
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("subscription fetch failed; cached hosts retained"))
    }

    pub async fn scan_once(&self) {
        let now = chrono::Utc::now().timestamp();
        let ids: Vec<_> = self
            .data
            .lock()
            .await
            .entries
            .iter()
            .filter(|e| {
                !e.url.is_empty()
                    && e.interval_seconds >= 60
                    && now.saturating_sub(e.last_attempt) >= e.interval_seconds as i64
            })
            .map(|e| e.id.clone())
            .collect();
        for id in ids {
            if let Err(error) = self.refresh(&id).await {
                eprintln!("subscription {id}: {error}");
            }
        }
    }

    pub async fn import(&self, revision: u64, mut document: Document) -> Result<()> {
        document.validate()?;
        let mut guard = self.data.lock().await;
        self.ready()?;
        guard.check_revision(revision)?;
        // Import config without silently changing host resolution. Existing
        // system snapshots stay local; imported history is not trusted input.
        for entry in &mut document.entries {
            entry.enabled = false;
            entry.last_attempt = 0;
            entry.last_success = 0;
            entry.last_error.clear();
        }
        ensure!(
            !guard.entries.iter().any(|e| e.enabled),
            "disable active entries before replacing the configuration"
        );
        document.history = guard.history.clone();
        document.revision = guard.revision + 1;
        self.save(&document)?;
        *guard = document;
        Ok(())
    }
}
