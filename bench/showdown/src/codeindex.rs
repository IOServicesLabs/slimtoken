//! The indexio arm (`SHOWDOWN_INDEX=indexio`; the default `text` uses the
//! engine's in-process `slimtoken::guard::TextIndex`). indexio replaces the
//! lookups of the repair guards (`guard`) only; the guards, the gate, the
//! model and the scheduler are the same in both arms.
//!
//! - **Index as files land.** Each closed wave file is mirrored to
//!   `<run>/_meta/indexio/src`; the first close runs `indexio index`, later
//!   ones `reindex --repo app --worktree`, at most one at a time and at
//!   least 250 ms apart, on a thread of its own (off the clock path).
//! - **Lookups.** The export lock asks `impact --file app:<path> --json` for
//!   the importers and call sites of a repaired module; after a round,
//!   `impact --diff-file <patch> --repo app --json` for the files that use a
//!   broken export. indexio finds importers by the file's basename, so each
//!   importer is confirmed by resolving its import. Each lookup (its wait
//!   for the index, a re-index and the query together) has a 2 s limit; on
//!   an error the text index answers and `fallbacks` counts it. After a
//!   timeout, or after two errors, indexio is off for the rest of the run
//!   (`down`): every later lookup is a fallback at once, and nothing is
//!   re-indexed. After each round the mirror catches up in the
//!   background; a lookup waits for it (that wait is on the clock path and
//!   counted).
//! - **What agreement measures.** An importer that indexio names is kept
//!   only if the text resolver confirms its import (`unconfirmed` counts
//!   the others, `named` all of them), and a used name with no indexio site
//!   takes the text index's lines (`sites_text`). So `agree` and `differ`
//!   measure indexio's recall of the confirmed importers, not an
//!   independent agreement: indexio can miss an importer but never add
//!   one.
//!
//! Every call passes `--data-dir <run>/_meta/indexio/data`: never the
//! default data directory, and never `add`, `sync`, `setup`, `hook` or
//! `sessions`. The executable is `SHOWDOWN_INDEXIO`, else the indexio
//! build beside this repository (`../indexio/target/release`) if there is
//! one, else the installed one (`~/.cargo/bin`). Each call is an
//! [`Ev::Tool`] event; `state.rs` adds them up for the SlimToken pane and
//! the results.

use crate::events::{Arm, Emitter, Ev};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use slimtoken::guard::{
    imported_from, text_affected, text_sites, text_users, CodeIndex, Site, User,
};
use slimtoken::profile::RepairHooks;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Longest wait for one indexio call.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Shortest time between two background re-indexes.
const REINDEX_GAP: Duration = Duration::from_millis(250);
/// The repo name of the app in the index.
const REPO: &str = "app";

/// True if `SHOWDOWN_INDEX` asks for indexio (default: `text`).
pub fn wanted() -> bool {
    std::env::var("SHOWDOWN_INDEX").is_ok_and(|value| value.trim() == "indexio")
}

/// The indexio executable: `SHOWDOWN_INDEXIO`, else the build beside this
/// repository, else the installed one, else `indexio` on the path.
pub fn exe() -> PathBuf {
    if let Some(path) = std::env::var_os("SHOWDOWN_INDEXIO").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) {
        "indexio.exe"
    } else {
        "indexio"
    };
    let beside = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../indexio/target/release")
        .join(name);
    if beside.is_file() {
        return beside;
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    if let Some(installed) = home
        .map(|home| PathBuf::from(home).join(".cargo").join("bin").join(name))
        .filter(|path| path.is_file())
    {
        return installed;
    }
    PathBuf::from(name)
}

/// What indexio did in one run (also in `slimtoken-repair.json`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexioStats {
    pub exe: String,
    /// The bare version (`0.1.15`); empty if `--version` failed.
    pub version: String,
    /// Why `--version` failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_error: Option<String>,
    /// Why indexio was turned off for the rest of the run (a timeout or a
    /// second error); every later lookup fell back to the text index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down: Option<String>,
    /// Errors of indexio calls (lookups and background index runs).
    pub errors: usize,
    /// Files in the mirror.
    pub files: usize,
    pub index_ms: u64,
    pub reindex: usize,
    pub reindex_ms: u64,
    /// `impact --file` lookups.
    pub impact: usize,
    pub impact_ms: u64,
    /// `impact --diff-file` lookups.
    pub diff: usize,
    pub diff_ms: u64,
    /// Time that the repair loop waited for indexio (lookups and the
    /// re-index they waited for).
    pub critical_ms: u64,
    pub fallbacks: usize,
    /// Lookups whose confirmed files are the text index's files (indexio's
    /// importers are confirmed by the text resolver first, so this is
    /// recall only: see the module notes).
    pub agree: usize,
    /// Lookups whose confirmed files differ from the text index's.
    pub differ: usize,
    /// Importers (and affected files) that indexio named, before
    /// confirming them.
    pub named: usize,
    /// Importers that indexio named but whose imports do not resolve to
    /// the module (dropped).
    pub unconfirmed: usize,
    /// Use sites of confirmed importers that came from indexio.
    pub sites_indexio: usize,
    /// Use sites taken from the text index (a used name with no indexio
    /// site).
    pub sites_text: usize,
}

impl IndexioStats {
    /// True if the arm did not run on indexio as asked: `--version`
    /// failed, indexio was turned off, or every lookup fell back.
    pub fn degraded(&self) -> bool {
        self.version.is_empty()
            || self.down.is_some()
            || (self.fallbacks > 0 && self.impact + self.diff == 0)
    }
}

/// The output of one call.
struct Output {
    ms: u64,
    stdout: String,
}

/// Run `exe args...` with the 2 s timeout.
fn run(exe: &Path, args: &[&str]) -> Result<Output> {
    let started = Instant::now();
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {}", exe.display()))?;
    // Read both pipes on their own threads: a full pipe never blocks the
    // child while this thread waits.
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("indexio {} timed out after {:?}", args.join(" "), TIMEOUT);
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        bail!(
            "indexio {} failed: {}",
            args.first().copied().unwrap_or_default(),
            stderr.trim().chars().take(300).collect::<String>()
        );
    }
    Ok(Output {
        ms: started.elapsed().as_millis() as u64,
        stdout,
    })
}

fn hash_of(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The state that the background thread and the lookups share.
struct Shared {
    exe: PathBuf,
    data: PathBuf,
    src: PathBuf,
    root: PathBuf,
    emitter: Emitter,
    arm: Arm,
    /// Mirrored path -> hash of its text.
    mirror: Mutex<BTreeMap<String, u64>>,
    indexed: AtomicBool,
    /// The mirror has changes that the index does not have.
    dirty: AtomicBool,
    /// One index or re-index at a time.
    busy: Mutex<Option<Instant>>,
    /// Off for the rest of the run (see [`Shared::failed`]).
    down: AtomicBool,
    patches: AtomicUsize,
    stats: Mutex<IndexioStats>,
}

impl Shared {
    fn emit(&self, op: &str, ms: u64, critical_ms: u64, files: usize, hits: usize, note: String) {
        self.emitter.emit(Ev::Tool {
            arm: self.arm,
            tool: "indexio".to_string(),
            op: op.to_string(),
            ms,
            critical_ms,
            files,
            hits,
            note,
        });
    }

    fn stats(&self) -> std::sync::MutexGuard<'_, IndexioStats> {
        self.stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_down(&self) -> bool {
        self.down.load(Ordering::SeqCst)
    }

    /// Count an error; a timeout or a second error turns indexio off for
    /// the rest of the run (once: the `down` event says why).
    fn failed(&self, error: &anyhow::Error) {
        let note = format!("{error:#}");
        let errors = {
            let mut stats = self.stats();
            stats.errors += 1;
            stats.errors
        };
        if (note.contains("timed out") || errors >= 2) && !self.down.swap(true, Ordering::SeqCst) {
            self.stats().down = Some(note.clone());
            self.emit("down", 0, 0, self.mirrored(), 0, note);
        }
    }

    fn data_arg(&self) -> String {
        self.data.display().to_string()
    }

    /// Write `text` to the mirror as `path` if it changed. True if it did.
    fn mirror(&self, path: &str, text: &str) -> bool {
        if path.contains("..") || path.starts_with('/') || path.contains(':') {
            return false;
        }
        let hash = hash_of(text);
        let mut mirror = self
            .mirror
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if mirror.get(path) == Some(&hash) {
            return false;
        }
        let target = self.src.join(path);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&target, text).is_err() {
            return false;
        }
        mirror.insert(path.to_string(), hash);
        self.dirty.store(true, Ordering::SeqCst);
        true
    }

    fn mirrored(&self) -> usize {
        self.mirror
            .lock()
            .map(|mirror| mirror.len())
            .unwrap_or_default()
    }

    /// Index or re-index the mirror if it changed; `critical` when the
    /// repair loop waits for it (no gap then). Returns the milliseconds
    /// spent, the wait for a call that ran already included.
    fn catch_up(&self, critical: bool) -> Result<u64> {
        let started = Instant::now();
        if self.is_down() {
            bail!("indexio is off");
        }
        if !critical {
            let last = *self
                .busy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(wait) = last.and_then(|last| REINDEX_GAP.checked_sub(last.elapsed())) {
                std::thread::sleep(wait);
            }
        }
        let mut busy = self
            .busy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.dirty.swap(false, Ordering::SeqCst) {
            // Nothing to index; a lookup may still have waited for the
            // background re-index that held the lock.
            let waited = started.elapsed().as_millis() as u64;
            if critical && waited > 0 {
                self.stats().critical_ms += waited;
                self.emit("wait", 0, waited, self.mirrored(), 0, String::new());
            }
            return Ok(waited);
        }
        let data = self.data_arg();
        let files = self.mirrored();
        let outcome = if self.indexed.load(Ordering::SeqCst) {
            run(
                &self.exe,
                &["--data-dir", &data, "reindex", "--repo", REPO, "--worktree"],
            )
            .map(|output| ("reindex", output.ms))
        } else {
            let src = self.src.display().to_string();
            run(
                &self.exe,
                &["--data-dir", &data, "index", &src, "--name", REPO],
            )
            .map(|output| ("index", output.ms))
        };
        *busy = Some(Instant::now());
        drop(busy);
        let (op, ms) = match outcome {
            Ok(done) => done,
            Err(error) => {
                // The next call tries again, unless this error turns
                // indexio off. (A lookup that waited for it counts the
                // error and the fallback.)
                self.dirty.store(true, Ordering::SeqCst);
                self.emit("error", 0, 0, files, 0, format!("{error:#}"));
                if !critical {
                    self.failed(&error);
                }
                return Err(error);
            }
        };
        self.indexed.store(true, Ordering::SeqCst);
        let total = started.elapsed().as_millis() as u64;
        let critical_ms = if critical { total } else { 0 };
        {
            let mut stats = self.stats();
            stats.files = files;
            if op == "index" {
                stats.index_ms += ms;
            } else {
                stats.reindex += 1;
                stats.reindex_ms += ms;
            }
            stats.critical_ms += critical_ms;
        }
        self.emit(op, ms, critical_ms, files, 0, String::new());
        Ok(total)
    }
}

/// indexio as the guards' [`CodeIndex`], fed as the files land.
pub struct Indexio {
    shared: Arc<Shared>,
    tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl Indexio {
    /// Start the arm in `root` (`<run>/_meta/indexio`, emptied first): the
    /// version check and the background thread.
    pub fn start(root: &Path, emitter: Emitter, arm: Arm) -> Result<Arc<Self>> {
        Self::start_with(root, exe(), emitter, arm)
    }

    /// [`Self::start`] with the executable `exe`.
    pub fn start_with(root: &Path, exe: PathBuf, emitter: Emitter, arm: Arm) -> Result<Arc<Self>> {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::create_dir_all(root.join("data"))?;
        let root = std::path::absolute(root)?;
        let shared = Arc::new(Shared {
            data: root.join("data"),
            src: root.join("src"),
            root,
            emitter,
            arm,
            mirror: Mutex::new(BTreeMap::new()),
            indexed: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            busy: Mutex::new(None),
            down: AtomicBool::new(false),
            patches: AtomicUsize::new(0),
            stats: Mutex::new(IndexioStats {
                exe: exe.display().to_string(),
                ..IndexioStats::default()
            }),
            exe,
        });
        let (tx, rx) = mpsc::channel::<()>();
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("indexio".to_string())
            .spawn(move || {
                // The note is `indexio v0.1.15`, or says why there is no
                // version (the arm is then marked degraded).
                let note = match run(&worker.exe, &["--version"]) {
                    Ok(output) => {
                        let version = output
                            .stdout
                            .split_whitespace()
                            .last()
                            .unwrap_or_default()
                            .trim_start_matches('v')
                            .to_string();
                        worker.stats().version = version.clone();
                        if version.is_empty() {
                            "indexio (no version)".to_string()
                        } else {
                            format!("indexio v{version}")
                        }
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        worker.stats().version_error = Some(error.clone());
                        format!("indexio (version check failed: {error})")
                    }
                };
                worker.emit("start", 0, 0, 0, 0, note);
                while rx.recv().is_ok() {
                    // One call for every signal that came in meanwhile.
                    while rx.try_recv().is_ok() {}
                    if !worker.is_down() {
                        let _ = worker.catch_up(false);
                    }
                }
            })?;
        Ok(Arc::new(Self {
            shared,
            tx: Mutex::new(Some(tx)),
        }))
    }

    /// A file landed (wave close, a rendered file): mirror it and let the
    /// background thread index it.
    pub fn land(&self, path: &str, text: &str) {
        if self.shared.mirror(path, text) && !self.shared.is_down() {
            self.kick();
        }
    }

    fn kick(&self) {
        if let Ok(tx) = self.tx.lock() {
            if let Some(tx) = tx.as_ref() {
                let _ = tx.send(());
            }
        }
    }

    /// What indexio did so far.
    pub fn stats(&self) -> IndexioStats {
        self.shared.stats().clone()
    }

    /// Stop the background thread (after the repair step).
    pub fn stop(&self) {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
    }

    /// Mirror `files` and bring the index up to date (on the clock path).
    async fn sync(&self, files: &BTreeMap<String, String>) -> Result<u64> {
        for (path, text) in files {
            self.shared.mirror(path, text);
        }
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.catch_up(true)).await?
    }

    /// Run one query on the blocking pool.
    async fn query(&self, args: Vec<String>) -> Result<Output> {
        let exe = self.shared.exe.clone();
        tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run(&exe, &args)
        })
        .await?
    }

    /// The text index answers: count it, and count the error (`error`
    /// is `None` when indexio is off already).
    fn fallback(&self, op: &str, path: &str, error: Option<&anyhow::Error>, waited: u64) {
        let note = match error {
            Some(error) => {
                self.shared.failed(error);
                format!("{op} {path}: {error:#}")
            }
            None => format!("{op} {path}: indexio is off"),
        };
        {
            let mut stats = self.shared.stats();
            stats.fallbacks += 1;
            stats.critical_ms += waited;
        }
        self.shared.emit("fallback", waited, waited, 0, 0, note);
    }

    /// `lookup` within the 2 s limit (the wait for the index, a re-index
    /// and the query together).
    async fn limited<T>(lookup: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(TIMEOUT, lookup).await {
            Ok(result) => result,
            Err(_) => bail!("indexio lookup timed out after {TIMEOUT:?}"),
        }
    }

    async fn impact_users(
        &self,
        path: &str,
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Result<(Vec<User>, u64, u64)> {
        let waited = self.sync(files).await?;
        let output = self
            .query(vec![
                "--data-dir".into(),
                self.shared.data_arg(),
                "impact".into(),
                "--file".into(),
                format!("{REPO}:{path}"),
                "--json".into(),
            ])
            .await?;
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).context("impact --file gave no JSON")?;
        let mut named: Vec<String> = Vec::new();
        for list in ["importers", "files"] {
            for entry in value[list].as_array().into_iter().flatten() {
                if let Some(file) = entry["path"].as_str() {
                    if file != path && !named.iter().any(|known| known == file) {
                        named.push(file.to_string());
                    }
                }
            }
        }
        let mut users = Vec::new();
        let mut unconfirmed = 0;
        let (mut from_indexio, mut from_text) = (0, 0);
        for file in &named {
            let Some(source) = files.get(file) else {
                unconfirmed += 1;
                continue;
            };
            let names = imported_from(file, source, path, files, hooks);
            if names.is_empty() {
                unconfirmed += 1;
                continue;
            }
            let exported: Vec<String> = names.iter().map(|(name, _)| name.clone()).collect();
            let mut sites: Vec<Site> = Vec::new();
            for site in value["sites"].as_array().into_iter().flatten() {
                let symbol = site["symbol"].as_str().unwrap_or_default();
                let wanted = site["path"].as_str() == Some(file.as_str())
                    && site["depth"].as_u64() == Some(1)
                    && exported.iter().any(|name| name == symbol);
                let kept = sites.iter().filter(|known| known.name == symbol).count();
                if wanted && kept < 3 {
                    sites.push(Site {
                        name: symbol.to_string(),
                        line: site["line"].as_u64().unwrap_or_default() as usize,
                        text: site["snippet"]
                            .as_str()
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        caller: site["caller"]
                            .as_str()
                            .filter(|caller| !caller.is_empty())
                            .map(str::to_string),
                    });
                }
            }
            // A name that indexio has no site for (a JSX render, a type)
            // keeps the text index's lines.
            let missing: Vec<(String, String)> = names
                .iter()
                .filter(|(name, _)| !sites.iter().any(|site| site.name == *name))
                .cloned()
                .collect();
            from_indexio += sites.len();
            let filled = text_sites(source, &missing);
            from_text += filled.len();
            sites.extend(filled);
            users.push(User {
                path: file.clone(),
                names: exported,
                sites,
            });
        }
        {
            let mut stats = self.shared.stats();
            stats.named += named.len();
            stats.unconfirmed += unconfirmed;
            stats.sites_indexio += from_indexio;
            stats.sites_text += from_text;
        }
        Ok((users, output.ms, waited))
    }

    /// A unified diff of `old` to `new` for `path`: one hunk from the
    /// first to the last changed line.
    fn patch(path: &str, old: &str, new: &str) -> String {
        let old: Vec<&str> = old.lines().collect();
        let new: Vec<&str> = new.lines().collect();
        let head = old
            .iter()
            .zip(&new)
            .take_while(|(left, right)| left == right)
            .count();
        let tail = old[head..]
            .iter()
            .rev()
            .zip(new[head..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        let removed = &old[head..old.len() - tail];
        let added = &new[head..new.len() - tail];
        let mut text = format!(
            "--- a/{path}\n+++ b/{path}\n@@ -{},{} +{},{} @@\n",
            head + 1,
            removed.len(),
            head + 1,
            added.len()
        );
        for line in removed {
            text.push('-');
            text.push_str(line);
            text.push('\n');
        }
        for line in added {
            text.push('+');
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    async fn diff_users(
        &self,
        path: &str,
        old: &str,
        new: &str,
        names: &[String],
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Result<(Vec<String>, u64)> {
        let number = self.shared.patches.fetch_add(1, Ordering::SeqCst);
        let patch = self.shared.root.join(format!("round-{number}.diff"));
        std::fs::write(&patch, Self::patch(path, old, new))?;
        let output = self
            .query(vec![
                "--data-dir".into(),
                self.shared.data_arg(),
                "impact".into(),
                "--diff-file".into(),
                patch.display().to_string(),
                "--repo".into(),
                REPO.into(),
                "--json".into(),
            ])
            .await?;
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).context("impact --diff-file gave no JSON")?;
        let mut found: BTreeSet<String> = BTreeSet::new();
        for list in ["importers", "files"] {
            for entry in value[list].as_array().into_iter().flatten() {
                if let Some(file) = entry["path"].as_str() {
                    found.insert(file.to_string());
                }
            }
        }
        let named = found.len();
        let confirmed: Vec<String> = found
            .into_iter()
            .filter(|file| {
                files.get(file).is_some_and(|source| {
                    imported_from(file, source, path, files, hooks)
                        .iter()
                        .any(|(name, _)| names.contains(name))
                })
            })
            .collect();
        {
            let mut stats = self.shared.stats();
            stats.named += named;
            stats.unconfirmed += named - confirmed.len();
        }
        Ok((confirmed, output.ms))
    }

    fn agreement(&self, same: bool) {
        let mut stats = self.shared.stats();
        if same {
            stats.agree += 1;
        } else {
            stats.differ += 1;
        }
    }
}

#[slimtoken::async_trait]
impl CodeIndex for Indexio {
    fn name(&self) -> &str {
        "indexio"
    }

    async fn users(
        &self,
        path: &str,
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Vec<User> {
        let started = Instant::now();
        let text = text_users(path, files, hooks);
        if self.shared.is_down() {
            self.fallback("impact", path, None, 0);
            return text;
        }
        match Self::limited(self.impact_users(path, files, hooks)).await {
            Ok((users, ms, waited)) => {
                // The wait for the index is counted by `catch_up`.
                let critical = (started.elapsed().as_millis() as u64).saturating_sub(waited);
                let sites: usize = users.iter().map(|user| user.sites.len()).sum();
                let ours: BTreeSet<&str> = users.iter().map(|user| user.path.as_str()).collect();
                let theirs: BTreeSet<&str> = text.iter().map(|user| user.path.as_str()).collect();
                self.agreement(ours == theirs);
                {
                    let mut stats = self.shared.stats();
                    stats.impact += 1;
                    stats.impact_ms += ms;
                    stats.critical_ms += critical;
                }
                self.shared.emit(
                    "impact",
                    ms,
                    critical,
                    users.len(),
                    sites,
                    format!(
                        "indexio impact {path} → {} importers, {sites} call sites ({ms} ms)",
                        users.len()
                    ),
                );
                users
            }
            Err(error) => {
                let waited = started.elapsed().as_millis() as u64;
                self.fallback("impact", path, Some(&error), waited);
                text
            }
        }
    }

    async fn affected(
        &self,
        path: &str,
        old: &str,
        new: &str,
        names: &[String],
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Vec<String> {
        let started = Instant::now();
        let text = text_affected(path, names, files, hooks);
        if self.shared.is_down() {
            self.fallback("diff", path, None, 0);
            return text;
        }
        match Self::limited(self.diff_users(path, old, new, names, files, hooks)).await {
            Ok((users, ms)) => {
                let critical = started.elapsed().as_millis() as u64;
                let ours: BTreeSet<&String> = users.iter().collect();
                let theirs: BTreeSet<&String> = text.iter().collect();
                self.agreement(ours == theirs);
                {
                    let mut stats = self.shared.stats();
                    stats.diff += 1;
                    stats.diff_ms += ms;
                    stats.critical_ms += critical;
                }
                self.shared.emit(
                    "diff",
                    ms,
                    critical,
                    users.len(),
                    names.len(),
                    format!(
                        "indexio diff {path} → {} broken, {} files use them ({ms} ms)",
                        names.len(),
                        users.len()
                    ),
                );
                users
            }
            Err(error) => {
                let waited = started.elapsed().as_millis() as u64;
                self.fallback("diff", path, Some(&error), waited);
                text
            }
        }
    }

    async fn updated(&self, files: &BTreeMap<String, String>) {
        // The round's files: mirror them now, index them in the background
        // while the gate runs (the next lookup waits for it).
        let mut changed = false;
        for (path, text) in files {
            changed |= self.shared.mirror(path, text);
        }
        if changed && !self.shared.is_down() {
            self.kick();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_indexio_falls_back_to_the_text_index() {
        let root = std::env::temp_dir().join(format!("showdown-codeindex-{}", std::process::id()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let index = Indexio::start_with(
            &root,
            PathBuf::from("no-such-indexio-executable"),
            Emitter::new(tx),
            Arm::Slim,
        )
        .unwrap();
        let files: BTreeMap<String, String> = [
            ("lib/a.ts", "export function f(x: string): void {}\n"),
            ("app/page.tsx", "import { f } from '../lib/a';\nf('x');\n"),
        ]
        .into_iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect();
        for (path, text) in &files {
            index.land(path, text);
        }
        let hooks = &slimtoken::profile::NEXTJS_REPAIR;
        let users = index.users("lib/a.ts", &files, hooks).await;
        assert_eq!(users, text_users("lib/a.ts", &files, hooks));
        let names = ["f".to_string()];
        let affected = index
            .affected("lib/a.ts", "a\n", "b\n", &names, &files, hooks)
            .await;
        assert_eq!(affected, ["app/page.tsx"]);
        index.stop();
        let stats = index.stats();
        assert_eq!(stats.fallbacks, 2);
        assert_eq!(stats.impact, 0);
        assert!(stats.degraded());
        // The mirror holds the files; the events say what failed.
        assert!(root.join("src/app/page.tsx").is_file());
        let mut fallbacks = 0;
        while let Ok(timed) = rx.try_recv() {
            if let Ev::Tool { op, .. } = timed.ev {
                fallbacks += usize::from(op == "fallback");
            }
        }
        assert_eq!(fallbacks, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An executable that hangs for 6 s whatever its arguments.
    fn hanging_exe(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        if cfg!(windows) {
            let path = dir.join("hang.cmd");
            std::fs::write(&path, "@ping -n 7 127.0.0.1 >nul\r\n").unwrap();
            path
        } else {
            let path = dir.join("hang.sh");
            std::fs::write(&path, "#!/bin/sh\nsleep 6\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hanging_indexio_costs_one_limit_then_is_off() {
        let base =
            std::env::temp_dir().join(format!("showdown-codeindex-hang-{}", std::process::id()));
        let exe = hanging_exe(&base.join("bin"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let index =
            Indexio::start_with(&base.join("run"), exe, Emitter::new(tx), Arm::Slim).unwrap();
        let files: BTreeMap<String, String> = [
            ("lib/a.ts", "export function f(x: string): void {}\n"),
            ("app/page.tsx", "import { f } from '../lib/a';\nf('x');\n"),
        ]
        .into_iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect();
        for (path, text) in &files {
            index.land(path, text);
        }
        let hooks = &slimtoken::profile::NEXTJS_REPAIR;
        let started = Instant::now();
        let users = index.users("lib/a.ts", &files, hooks).await;
        let first = started.elapsed();
        assert_eq!(users, text_users("lib/a.ts", &files, hooks));
        // The whole lookup (wait, index, query) has one 2 s limit.
        assert!(
            first < Duration::from_millis(3_000),
            "first lookup {first:?}"
        );
        // Off now: the next lookups answer at once from the text index.
        let started = Instant::now();
        let users = index.users("lib/a.ts", &files, hooks).await;
        assert_eq!(users, text_users("lib/a.ts", &files, hooks));
        let names = ["f".to_string()];
        let affected = index
            .affected("lib/a.ts", "a\n", "b\n", &names, &files, hooks)
            .await;
        assert_eq!(affected, ["app/page.tsx"]);
        let later = started.elapsed();
        assert!(
            later < Duration::from_millis(200),
            "later lookups {later:?}"
        );
        index.stop();
        let stats = index.stats();
        assert_eq!(stats.fallbacks, 3);
        assert!(stats
            .down
            .as_deref()
            .is_some_and(|why| why.contains("timed out")));
        assert!(stats.degraded());
        let mut downs = 0;
        while let Ok(timed) = rx.try_recv() {
            if let Ev::Tool { op, .. } = timed.ev {
                downs += usize::from(op == "down");
            }
        }
        assert_eq!(downs, 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_patch_is_one_hunk_around_the_change() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\n";
        assert_eq!(
            Indexio::patch("lib/x.ts", old, new),
            "--- a/lib/x.ts\n+++ b/lib/x.ts\n@@ -2,1 +2,1 @@\n-b\n+B\n"
        );
        let grown = Indexio::patch("x.ts", "a\n", "a\nb\n");
        assert!(grown.ends_with("@@ -2,0 +2,1 @@\n+b\n"));
    }
}

/// Runs on copies of saved apps, with no model (`cargo test --release --
/// --ignored dry_`): `SHOWDOWN_DRY_APPS` lists the app directories (`;`),
/// `SHOWDOWN_DRY_OUT` a scratch directory (the indexio data and the gate's
/// work directories go there), `SHOWDOWN_DRY_GATE` the gate command.
#[cfg(test)]
mod dry {
    use super::*;
    use crate::events::Timed;
    use slimtoken::dag::{Plan, Task};
    use slimtoken::profile::NEXTJS_REPAIR;
    use slimtoken::scheduler::{execute_plan, RunReport, SchedulerConfig};
    use slimtoken::{Completion, CompletionRequest, ModelClient};

    fn apps() -> Vec<PathBuf> {
        std::env::var("SHOWDOWN_DRY_APPS")
            .unwrap_or_default()
            .split(';')
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from)
            .collect()
    }

    fn out() -> PathBuf {
        PathBuf::from(std::env::var("SHOWDOWN_DRY_OUT").expect("SHOWDOWN_DRY_OUT"))
    }

    /// The source files of an app, as the bench's `rerepair` reads them.
    fn app_files(app: &Path) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        let mut dirs = vec![app.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if path.is_dir() {
                    if !matches!(name.as_str(), "node_modules" | ".next" | "_meta" | ".check") {
                        dirs.push(path);
                    }
                    continue;
                }
                let source = [".ts", ".tsx", ".css", ".mjs", ".js", ".json"]
                    .iter()
                    .any(|end| name.ends_with(end));
                if !source || name == "package-lock.json" || name == "next-env.d.ts" {
                    continue;
                }
                let rel = path
                    .strip_prefix(app)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(rel, std::fs::read_to_string(&path).unwrap());
            }
        }
        files
    }

    fn emitter() -> (Emitter, tokio::sync::mpsc::UnboundedReceiver<Timed>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Emitter::new(tx), rx)
    }

    fn is_code(path: &str) -> bool {
        (path.ends_with(".ts") || path.ends_with(".tsx")) && !(NEXTJS_REPAIR.is_test)(path)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn dry_indexio_agrees_with_the_text_index() {
        let hooks = &NEXTJS_REPAIR;
        for app in apps() {
            let name = app.file_name().unwrap().to_string_lossy().to_string();
            let files = app_files(&app);
            // A cold index of the whole app.
            let (cold_emitter, _cold_rx) = emitter();
            let cold =
                Indexio::start(&out().join(&name).join("cold"), cold_emitter, Arm::Slim).unwrap();
            for (path, text) in &files {
                cold.shared.mirror(path, text);
            }
            cold.sync(&files).await.unwrap();
            let one = files.keys().find(|path| is_code(path)).unwrap().clone();
            let mut touched = files.clone();
            touched.get_mut(&one).unwrap().push_str("\n// touched\n");
            cold.sync(&touched).await.unwrap();
            let cold_stats = cold.stats();
            cold.stop();
            // As in a run: the files land one by one, then the lookups.
            let (live_emitter, _live_rx) = emitter();
            let index = Indexio::start(&out().join(&name).join("indexio"), live_emitter, Arm::Slim)
                .unwrap();
            for (path, text) in &files {
                index.land(path, text);
                std::thread::sleep(Duration::from_millis(20));
            }
            let (mut modules, mut agree, mut differ) = (0, 0, 0);
            let (mut text_importers, mut index_importers) = (0, 0);
            let (mut text_sites, mut index_sites, mut callers) = (0, 0, 0);
            for path in files.keys().filter(|path| is_code(path)) {
                let text = text_users(path, &files, hooks);
                if text.is_empty() {
                    continue;
                }
                modules += 1;
                let found = index.users(path, &files, hooks).await;
                let ours: BTreeSet<&str> = found.iter().map(|user| user.path.as_str()).collect();
                let theirs: BTreeSet<&str> = text.iter().map(|user| user.path.as_str()).collect();
                if ours == theirs {
                    agree += 1;
                } else {
                    differ += 1;
                    println!(
                        "  differ {path}: text-only {:?} indexio-only {:?}",
                        theirs.difference(&ours).collect::<Vec<_>>(),
                        ours.difference(&theirs).collect::<Vec<_>>()
                    );
                }
                text_importers += theirs.len();
                index_importers += ours.len();
                text_sites += text.iter().map(|user| user.sites.len()).sum::<usize>();
                index_sites += found.iter().map(|user| user.sites.len()).sum::<usize>();
                callers += found
                    .iter()
                    .flat_map(|user| &user.sites)
                    .filter(|site| site.caller.is_some())
                    .count();
            }
            // A broken export: the first locked one of the module with the
            // most users gets another parameter type.
            let module = files
                .keys()
                .filter(|path| is_code(path))
                .max_by_key(|path| text_users(path, &files, hooks).len())
                .unwrap()
                .clone();
            let users = text_users(&module, &files, hooks);
            let lock = slimtoken::guard::Lock::new(&module, &files[&module], users, hooks);
            let mut diff_note = String::from("no lock");
            if let Some(lock) = lock {
                let (broken_name, decl) = lock.decls.iter().next().unwrap();
                let old = files[&module].clone();
                let first_line = decl
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ");
                let new = old.replacen(&first_line, &format!("{first_line}Changed"), 1);
                let mut after = files.clone();
                after.insert(module.clone(), new.clone());
                let names = lock.broken(&new, hooks);
                let found = index
                    .affected(&module, &old, &new, &names, &after, hooks)
                    .await;
                let text = text_affected(&module, &names, &after, hooks);
                diff_note = format!(
                    "{module} `{broken_name}` broken {:?}: indexio {} files, text {} files, same {}",
                    names,
                    found.len(),
                    text.len(),
                    found.iter().collect::<BTreeSet<_>>() == text.iter().collect::<BTreeSet<_>>()
                );
            }
            let stats = index.stats();
            index.stop();
            println!(
                "{name}: v{} files {} | cold index {} ms, reindex {} ms | live: index {} ms, {} reindex {} ms | {} modules with importers: agree {agree} differ {differ} (after confirming each importer with the text resolver: recall only) | importers text {text_importers} indexio confirmed {index_importers}, named raw {} (unconfirmed {}) | sites text {text_sites} lock {index_sites}: from indexio {}, filled from text {} (with caller {callers}) | {} impact {} ms, {} diff {} ms, on clock {} ms, fallbacks {}, down {:?} | {diff_note}",
                stats.version,
                files.len(),
                cold_stats.index_ms,
                cold_stats.reindex_ms,
                stats.index_ms,
                stats.reindex,
                stats.reindex_ms,
                modules,
                stats.named,
                stats.unconfirmed,
                stats.sites_indexio,
                stats.sites_text,
                stats.impact,
                stats.impact_ms,
                stats.diff,
                stats.diff_ms,
                stats.critical_ms,
                stats.fallbacks,
                stats.down,
            );
        }
    }

    /// A client that must not be called: the dry runs ask no model. Each
    /// request is counted (and fails), so a dry run shows how many repairs
    /// a live run would have sent.
    #[derive(Default)]
    struct NoModel {
        calls: AtomicUsize,
        titles: Mutex<Vec<String>>,
    }

    #[slimtoken::async_trait]
    impl ModelClient for NoModel {
        async fn complete(&self, request: CompletionRequest) -> anyhow::Result<Completion> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let title = request
                .user
                .lines()
                .find_map(|line| line.strip_prefix("Title: "))
                .or_else(|| request.user.lines().next())
                .unwrap_or("?")
                .chars()
                .take(80)
                .collect::<String>();
            if let Ok(mut titles) = self.titles.lock() {
                titles.push(title);
            }
            anyhow::bail!("dry run: no model")
        }
    }

    /// The plan and the report of an app's files, as `rerepair` builds them.
    async fn plan_of(files: &BTreeMap<String, String>) -> (Plan, RunReport) {
        let mut plan = Plan {
            objective: "dry run".to_string(),
            tasks: Vec::new(),
            cache_primes: Vec::new(),
        };
        for (index, (path, text)) in files.iter().enumerate() {
            plan.tasks.push(Task {
                id: format!("f{index}"),
                title: path.clone(),
                prompt: format!("Title: {path}"),
                deps: Vec::new(),
                max_tokens: 64,
                temperature: None,
                require_closed_fences: false,
                validate_artifact: false,
                static_output: Some(slimtoken::project::fenced(text)),
                reasoning_effort: None,
                internal: false,
                prefix_split: None,
            });
        }
        let client: Arc<dyn ModelClient> = Arc::new(NoModel::default());
        let report = execute_plan(&plan, client, SchedulerConfig::default())
            .await
            .unwrap();
        for task in &mut plan.tasks {
            task.static_output = None;
        }
        (plan, report)
    }

    /// The locked export of `files` with the most users, unexported in its
    /// module: its importers then report TS2459.
    fn unexport(files: &mut BTreeMap<String, String>) -> Option<(String, String, usize)> {
        let hooks = &NEXTJS_REPAIR;
        let mut best: Option<(String, String, usize)> = None;
        for path in files.keys().filter(|path| is_code(path)) {
            let users = text_users(path, files, hooks);
            for export in (hooks.guard.exports)(&files[path]) {
                if export.name == "default" || export.decl.starts_with("export {") {
                    continue;
                }
                let count = users
                    .iter()
                    .filter(|user| user.names.contains(&export.name))
                    .filter(|user| !(hooks.is_test)(&user.path))
                    .count();
                if count >= 2 && best.as_ref().is_none_or(|(_, _, known)| count > *known) {
                    best = Some((path.clone(), export.name.clone(), count));
                }
            }
        }
        let (path, name, count) = best?;
        let text = files.get_mut(&path)?;
        let line = text
            .lines()
            .find(|line| line.starts_with("export ") && line.contains(&format!(" {name}")))?
            .to_string();
        *text = text.replacen(&line, line.trim_start_matches("export "), 1);
        Some((path, name, count))
    }

    /// The layout without the first provider it renders.
    fn unmount(files: &mut BTreeMap<String, String>) -> Option<String> {
        let layout = files.get("app/layout.tsx")?.clone();
        let provider = files
            .keys()
            .filter(|path| path.ends_with("Provider.tsx"))
            .filter_map(|path| path.rsplit('/').next()?.strip_suffix(".tsx"))
            .find(|name| layout.contains(&format!("<{name}>")))?
            .to_string();
        let without: Vec<&str> = layout
            .lines()
            .filter(|line| !(line.trim_start().starts_with("import ") && line.contains(&provider)))
            .collect();
        let text = without
            .join("\n")
            .replace(&format!("<{provider}>"), "")
            .replace(&format!("</{provider}>"), "");
        files.insert("app/layout.tsx".to_string(), text);
        Some(provider)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn dry_guards_with_the_gate() {
        let gate = std::env::var("SHOWDOWN_DRY_GATE").expect("SHOWDOWN_DRY_GATE");
        let work = out().join("gate");
        std::fs::create_dir_all(&work).unwrap();
        std::env::set_current_dir(&work).unwrap();
        let use_indexio = std::env::var("SHOWDOWN_DRY_INDEX").is_ok_and(|value| value == "indexio");
        for app in apps() {
            let name = app.file_name().unwrap().to_string_lossy().to_string();
            let clean = app_files(&app);
            for scenario in ["unexport", "unmount"] {
                let mut files = clean.clone();
                let what = match scenario {
                    "unexport" => unexport(&mut files).map(|(path, name, count)| {
                        format!("{name} unexported in {path} ({count} importers)")
                    }),
                    _ => {
                        unmount(&mut files).map(|provider| format!("{provider} out of the layout"))
                    }
                };
                let Some(what) = what else {
                    println!("{name} {scenario}: no case in this app");
                    continue;
                };
                let (mut plan, mut report) = plan_of(&files).await;
                let index: Option<Arc<dyn CodeIndex>> = if use_indexio {
                    let (live_emitter, _rx) = emitter();
                    let index = Indexio::start(
                        &out().join(&name).join(format!("gate-{scenario}")),
                        live_emitter,
                        Arm::Slim,
                    )
                    .unwrap();
                    for (path, text) in &files {
                        index.land(path, text);
                    }
                    Some(index)
                } else {
                    None
                };
                crate::guard::take_layout_fixes();
                let started = Instant::now();
                let model = Arc::new(NoModel::default());
                let summary = crate::repair::run_guarded(
                    &mut plan,
                    &mut report,
                    model.clone(),
                    SchedulerConfig::default(),
                    &gate,
                    2,
                    None,
                    index,
                )
                .await;
                let layout = crate::guard::take_layout_fixes();
                let calls = model.calls.load(Ordering::SeqCst);
                let asked = model
                    .titles
                    .lock()
                    .map(|titles| titles.clone())
                    .unwrap_or_default();
                match summary {
                    Ok((summary, _)) => println!(
                        "{name} {scenario} ({what}) guard {}: before {} problems{}, after {} (passed {}), rounds {}, model requests {calls} {asked:?}, repaired {:?}, layout fixes {layout}, {:?}, {:.1} s",
                        crate::arms::step_on("guard"),
                        summary.before.failed,
                        summary
                            .before
                            .problems
                            .values()
                            .flat_map(|text| text.lines())
                            .filter(|line| line.contains("error TS"))
                            .map(|line| line.split("error ").nth(1).unwrap_or("").chars().take(7).collect::<String>())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .map(|code| format!(" {code}"))
                            .collect::<String>(),
                        summary.after.failed,
                        summary.after.passed,
                        summary.rounds.len(),
                        summary.repaired,
                        summary.guard,
                        started.elapsed().as_secs_f64()
                    ),
                    Err(error) => println!("{name} {scenario}: error {error:#}"),
                }
            }
        }
    }
}
