use crate::architect::{generate_manifest_plan_with_options, ArchitectOptions, PlanOptions};
use crate::client::ModelClient;
use crate::control::{
    unix_time_ms, validate_steering, RunControl, TaskProgress, TaskProgressStatus,
};
use crate::dag::Plan;
use crate::pipeline::{build, BuildHooks, BuildOptions, Cancelled, PlanSource};
use crate::planner::{generate_plan_with_steering, mock_plan};
use crate::profile::{DurationProfile, MaxTokensProfile, Profile};
use crate::scheduler::{ProbeConfig, RunReport, SchedulerConfig, SharedLimits, TaskStatus};
use anyhow::{bail, Context, Result};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchedulerOverrides {
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub retry_base_delay_ms: Option<u64>,
    #[serde(default)]
    pub task_timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_dependency_chars: Option<usize>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub adaptive: Option<bool>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Cross-session shared context spliced into the byte-identical prefix.
    #[serde(default)]
    pub shared_context_extra: Option<String>,
    /// Prefix-cache routing key forwarded to the server.
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    /// Request-start rate cap (token bucket per minute).
    #[serde(default)]
    pub max_starts_per_minute: Option<u32>,
    /// Estimated-token spend cap per minute (TPM pacing, leaky bucket
    /// with completion refunds). Distinct from the request-start cap.
    #[serde(default)]
    pub max_tokens_per_minute: Option<u64>,
    /// Rate-limit auto-tuner: shrink/recover the pace caps from
    /// typed 429 observations; the configured cap stays the ceiling.
    #[serde(default)]
    pub rate_auto_tune: Option<bool>,
    /// Rate-aware retry gating: typed-429 retries wait on the paced
    /// bucket instead of the fixed backoff ladder. Requires
    /// `max_starts_per_minute` in the resolved config.
    #[serde(default)]
    pub rate_retry_gating: Option<bool>,
    /// Max_tokens right-sizing profile: caps task output budgets at
    /// plan load. Distinct from --max-tokens-style global caps: per-class,
    /// matched on the task title, and never raises a plan-authored budget.
    #[serde(default)]
    pub max_tokens_profile: Option<MaxTokensProfile>,
    /// Duration-prediction profile: longest-predicted-first admission
    /// order within a critical-path depth, from historical task durations.
    #[serde(default)]
    pub duration_profile: Option<DurationProfile>,
    /// Concurrency auto-probe: ladder of tiny 1-token waves at wave
    /// start detecting the provider's TTFT/saturation/failure knee; the
    /// detected width seeds the adaptive controller's cold-start caps.
    #[serde(default)]
    pub concurrency_probe: Option<ProbeConfig>,
    /// Cap on in-flight in-run repairs: queued repairs start only
    /// from slots fresh tasks did not take, instead of bypassing the
    /// admission ceiling on every failure.
    #[serde(default)]
    pub max_concurrent_repairs: Option<usize>,
    /// Patience bound for capped repairs in milliseconds: a queued
    /// repair that has waited this long starts from slots fresh tasks would
    /// otherwise claim. Requires max_concurrent_repairs.
    #[serde(default)]
    pub repair_inject_after_ms: Option<u64>,
    /// Feed the adaptive controller a health sample at each request's first
    /// token instead of only at completion.
    #[serde(default)]
    pub controller_early_ttft: Option<bool>,
    /// First-token watchdog factor: abandon and retry an attempt with no
    /// first token after this multiple of the wave's median TTFT.
    #[serde(default)]
    pub first_token_watchdog: Option<f64>,
}

impl SchedulerOverrides {
    pub fn resolve(&self, base: &SchedulerConfig) -> SchedulerConfig {
        SchedulerConfig {
            max_concurrency: self.max_concurrency.unwrap_or(base.max_concurrency),
            max_retries: self.max_retries.unwrap_or(base.max_retries),
            retry_base_delay: self
                .retry_base_delay_ms
                .map(Duration::from_millis)
                .unwrap_or(base.retry_base_delay),
            task_timeout: self
                .task_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(base.task_timeout),
            max_dependency_chars: self
                .max_dependency_chars
                .unwrap_or(base.max_dependency_chars),
            default_temperature: self.temperature.unwrap_or(base.default_temperature),
            adaptive: self.adaptive.unwrap_or(base.adaptive),
            controller: base.controller.clone(),
            reasoning_effort: self
                .reasoning_effort
                .clone()
                .or_else(|| base.reasoning_effort.clone()),
            escalate_on_repair: base.escalate_on_repair,
            shared_context_extra: self
                .shared_context_extra
                .clone()
                .or_else(|| base.shared_context_extra.clone()),
            prompt_cache_key: self
                .prompt_cache_key
                .clone()
                .or_else(|| base.prompt_cache_key.clone()),
            max_starts_per_minute: self.max_starts_per_minute.or(base.max_starts_per_minute),
            max_tokens_per_minute: self.max_tokens_per_minute.or(base.max_tokens_per_minute),
            rate_auto_tune: self.rate_auto_tune.unwrap_or(base.rate_auto_tune),
            rate_retry_gating: self.rate_retry_gating.unwrap_or(base.rate_retry_gating),
            max_tokens_profile: self
                .max_tokens_profile
                .clone()
                .or_else(|| base.max_tokens_profile.clone()),
            duration_profile: self
                .duration_profile
                .clone()
                .or_else(|| base.duration_profile.clone()),
            concurrency_probe: self
                .concurrency_probe
                .clone()
                .or_else(|| base.concurrency_probe.clone()),
            max_concurrent_repairs: self.max_concurrent_repairs.or(base.max_concurrent_repairs),
            repair_inject_after_ms: self.repair_inject_after_ms.or(base.repair_inject_after_ms),
            validator: base.validator.clone(),
            cache_warm_gate: base.cache_warm_gate,
            cache_warm_poll: base.cache_warm_poll,
            cache_warm_budget: base.cache_warm_budget,
            trace_path: base.trace_path.clone(),
            trace_append: base.trace_append,
            repair_error_chars: base.repair_error_chars,
            planning: base.planning.clone(),
            controller_early_ttft: self
                .controller_early_ttft
                .unwrap_or(base.controller_early_ttft),
            first_token_watchdog: self.first_token_watchdog.or(base.first_token_watchdog),
            journal_path: base.journal_path.clone(),
            shared_limits: base.shared_limits.clone(),
            continue_truncated: base.continue_truncated,
            continue_runaway_tokens: base.continue_runaway_tokens,
            file_min_tokens: base.file_min_tokens,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunRequest {
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub steering: Vec<String>,
    #[serde(default)]
    pub max_tasks: Option<usize>,
    #[serde(default)]
    pub plan_only: bool,
    /// Greenfield mode: one cheap architect call fixes the file manifest and
    /// interface contracts, tasks are built deterministically with zero
    /// dependency edges (one parallel wave), and no model sink is needed.
    #[serde(default)]
    pub architect: bool,
    #[serde(default)]
    pub scheduler: Option<SchedulerOverrides>,
    /// Architect mode: stack profile by name (`Profile::named`); `None`
    /// detects it from the objective. Setting it (or `plan_options`) plans
    /// like the CLI, which also runs the profile's `normalize_manifest`.
    #[serde(default)]
    pub profile: Option<String>,
    /// Architect mode: plan-shaping knobs. Setting the field at all, even to
    /// `{}`, plans like the CLI (with `normalize_manifest`); leaving it out
    /// together with `profile` builds the plan exactly as before.
    #[serde(default)]
    pub plan_options: Option<PlanOptions>,
}

/// Highest `plan_options.quota_pct` a request may ask for (10x the demand).
const MAX_QUOTA_PCT: u64 = 1000;

impl RunRequest {
    pub fn validate(&self) -> Result<()> {
        match (&self.objective, &self.plan) {
            (Some(objective), None) if !objective.trim().is_empty() => {}
            (None, Some(plan)) => plan.validate()?,
            (Some(_), None) => bail!("objective cannot be empty"),
            (Some(_), Some(_)) => bail!("provide objective or plan, not both"),
            (None, None) => bail!("provide objective or plan"),
        }
        validate_steering(&self.steering)?;
        if let Some(max_tasks) = self.max_tasks {
            if !(1..=256).contains(&max_tasks) {
                bail!("max_tasks must be between 1 and 256");
            }
        }
        if let Some(overrides) = &self.scheduler {
            overrides.resolve(&SchedulerConfig::default()).validate()?;
        }
        if !self.architect && (self.profile.is_some() || self.plan_options.is_some()) {
            bail!("profile and plan_options need architect mode");
        }
        if let Some(name) = &self.profile {
            if Profile::named(name).is_none() {
                bail!("unknown profile {name:?}");
            }
        }
        if let Some(options) = &self.plan_options {
            if options.quota_pct > MAX_QUOTA_PCT {
                bail!("plan_options.quota_pct must be at most {MAX_QUOTA_PCT}");
            }
            if options.min_test_shards > 256 {
                bail!("plan_options.min_test_shards must be at most 256");
            }
            if let Some((year, month, day)) = options.today {
                if !(1..=9999).contains(&year)
                    || !(1..=12).contains(&month)
                    || !(1..=31).contains(&day)
                {
                    bail!("plan_options.today must be [year, month, day] with year 1-9999, month 1-12 and day 1-31");
                }
            }
        }
        Ok(())
    }

    fn scheduler_config(&self, base: &SchedulerConfig) -> Result<SchedulerConfig> {
        let config = self
            .scheduler
            .as_ref()
            .map(|overrides| overrides.resolve(base))
            .unwrap_or_else(|| base.clone());
        config.validate()?;
        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Planning,
    Running,
    Cancelling,
    PlanReady,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::PlanReady | Self::Succeeded | Self::Failed | Self::Cancelled
        )
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Queued => "queued",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::PlanReady => "plan_ready",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: String,
    pub objective: String,
    pub status: RunStatus,
    pub plan_only: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub task_count: usize,
    pub pending: usize,
    pub running: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cancelled: usize,
    pub steering_count: usize,
    pub has_plan: bool,
    pub has_report: bool,
    pub error: Option<String>,
    pub output_dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDetails {
    pub summary: RunSummary,
    pub steering: Vec<String>,
    pub tasks: BTreeMap<String, TaskProgress>,
}

#[derive(Debug)]
struct RunState {
    status: RunStatus,
    updated_at_ms: u64,
    error: Option<String>,
}

#[derive(Debug)]
struct RunRecord {
    id: String,
    objective: String,
    plan_only: bool,
    created_at_ms: u64,
    output_dir: PathBuf,
    control: RunControl,
    state: RwLock<RunState>,
    plan: RwLock<Option<Plan>>,
    report: RwLock<Option<RunReport>>,
}

impl RunRecord {
    fn set_state(&self, status: RunStatus, error: Option<String>) {
        let mut state = self.state.write().expect("run state lock");
        state.status = status;
        state.updated_at_ms = unix_time_ms();
        state.error = error;
    }
}

struct ManagerInner {
    client: Arc<dyn ModelClient>,
    base_config: SchedulerConfig,
    output_root: PathBuf,
    mock_planner: bool,
    runs: RwLock<HashMap<String, Arc<RunRecord>>>,
    /// One set of rate and in-flight budgets for every run of this manager:
    /// they all talk to the same provider through `client`, which enforces
    /// its limits per key, not per run.
    limits: SharedLimits,
}

/// Finished runs kept in memory for status and report queries. Older
/// finished runs are evicted (their plan.json and report.json stay on disk);
/// a long-lived server would otherwise hold every report it ever produced.
const MAX_RETAINED_RUNS: usize = 256;

/// Shared asynchronous run store used by both the interactive CLI and REST API.
#[derive(Clone)]
pub struct RunManager {
    inner: Arc<ManagerInner>,
}

impl RunManager {
    pub fn new(
        client: Arc<dyn ModelClient>,
        base_config: SchedulerConfig,
        output_root: PathBuf,
        mock_planner: bool,
    ) -> Self {
        let limits = base_config.shared_limits.clone().unwrap_or_else(|| {
            SharedLimits::new(
                base_config.max_concurrency,
                base_config.max_starts_per_minute,
                base_config.max_tokens_per_minute,
            )
        });
        Self {
            inner: Arc::new(ManagerInner {
                client,
                base_config,
                output_root,
                mock_planner,
                runs: RwLock::new(HashMap::new()),
                limits,
            }),
        }
    }

    /// Drop the oldest finished runs beyond `MAX_RETAINED_RUNS`. Active runs
    /// are never evicted.
    fn evict_finished_runs(&self) {
        let mut runs = self.inner.runs.write().expect("run store lock");
        if runs.len() <= MAX_RETAINED_RUNS {
            return;
        }
        let mut finished: Vec<(u64, String)> = runs
            .values()
            .filter(|record| {
                record
                    .state
                    .read()
                    .expect("run state lock")
                    .status
                    .is_terminal()
            })
            .map(|record| (record.created_at_ms, record.id.clone()))
            .collect();
        finished.sort();
        let excess = runs.len() - MAX_RETAINED_RUNS;
        for (_, id) in finished.into_iter().take(excess) {
            runs.remove(&id);
        }
    }

    pub async fn submit(&self, request: RunRequest) -> Result<RunSummary> {
        request.validate()?;
        let config = request.scheduler_config(&self.inner.base_config)?;
        let objective = request
            .objective
            .clone()
            .or_else(|| request.plan.as_ref().map(|plan| plan.objective.clone()))
            .context("validated request has no objective")?;
        let control = RunControl::with_steering(request.steering.clone())?;
        tokio::fs::create_dir_all(&self.inner.output_root)
            .await
            .with_context(|| format!("failed to create {}", self.inner.output_root.display()))?;

        let id = new_run_id();
        let output_dir = self.inner.output_root.join(&id);
        tokio::fs::create_dir_all(&output_dir)
            .await
            .with_context(|| format!("failed to create {}", output_dir.display()))?;
        let now = unix_time_ms();
        let record = Arc::new(RunRecord {
            id: id.clone(),
            objective,
            plan_only: request.plan_only,
            created_at_ms: now,
            output_dir,
            control,
            state: RwLock::new(RunState {
                status: RunStatus::Queued,
                updated_at_ms: now,
                error: None,
            }),
            plan: RwLock::new(None),
            report: RwLock::new(None),
        });
        let summary = self.summary(&record);
        self.inner
            .runs
            .write()
            .expect("run store lock")
            .insert(id, Arc::clone(&record));
        self.evict_finished_runs();

        let manager = self.clone();
        tokio::spawn(async move {
            let result = AssertUnwindSafe(manager.execute(Arc::clone(&record), request, config))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let status = if record.control.is_cancelled() {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Failed
                    };
                    record.set_state(status, Some(format!("{error:#}")));
                }
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    record.set_state(
                        RunStatus::Failed,
                        Some(format!("run execution panicked: {message}")),
                    );
                }
            }
        });
        Ok(summary)
    }

    async fn execute(
        &self,
        record: Arc<RunRecord>,
        request: RunRequest,
        config: SchedulerConfig,
    ) -> Result<()> {
        record.set_state(RunStatus::Planning, None);
        debug!(run_id = %record.id, mock = self.inner.mock_planner, "planning run");
        let cancelled = || {
            record.set_state(
                RunStatus::Cancelled,
                Some("cancelled during planning".into()),
            );
        };
        // Every path plans through `pipeline::build`: the plans this module
        // makes itself go in as given plans, and a request that names a
        // profile or plan options plans in `build` (`plan_project`).
        let mut options = BuildOptions {
            control: record.control.clone(),
            ..BuildOptions::default()
        };
        if let Some(plan) = request.plan.clone() {
            options.source = PlanSource::Given(plan);
        } else {
            let objective = request.objective.as_deref().context("objective missing")?;
            if self.inner.mock_planner {
                options.source = PlanSource::Given(mock_plan(objective));
            } else if request.architect {
                // Size router: deliverables that fit one model response go
                // out as a single direct call (the single-stream token
                // optimum); only oversized objectives pay for the manifest
                // call and the parallel wave.
                if crate::architect::fits_single_response(objective) {
                    options.source =
                        PlanSource::Given(crate::architect::direct_response_plan(objective));
                } else {
                    // Tighter default budget than the general planner: a
                    // greenfield build should be ~10 one-file workers, not 48.
                    let max_tasks = request.max_tasks.unwrap_or(12);
                    let cache_key = request
                        .scheduler
                        .as_ref()
                        .and_then(|overrides| overrides.prompt_cache_key.clone());
                    // A request that names a profile or plan options plans
                    // through `plan_project` like the CLI, which also runs
                    // the profile's `normalize_manifest`. Without them the
                    // plan is built exactly as before.
                    if request.profile.is_none() && request.plan_options.is_none() {
                        let planning = generate_manifest_plan_with_options(
                            self.inner.client.as_ref(),
                            objective,
                            max_tasks,
                            &request.steering,
                            // Split fragments by default (see the CLI's
                            // --architect-single-shot for the measurement).
                            true,
                            cache_key.as_deref(),
                        );
                        tokio::select! {
                            _ = record.control.cancelled() => {
                                cancelled();
                                return Ok(());
                            }
                            result = planning => options.source = PlanSource::Given(result?),
                        }
                    } else {
                        options.profile = match &request.profile {
                            Some(name) => Some(
                                Profile::named(name)
                                    .with_context(|| format!("unknown profile {name:?}"))?,
                            ),
                            None => None,
                        };
                        options.max_tasks = max_tasks;
                        options.steering = request.steering.clone();
                        options.architect = ArchitectOptions {
                            force_split: true,
                            cache_key,
                            ..ArchitectOptions::default()
                        };
                        options.plan = request.plan_options.unwrap_or_default();
                    }
                }
            } else {
                let max_tasks = request.max_tasks.unwrap_or(48);
                tokio::select! {
                    _ = record.control.cancelled() => {
                        cancelled();
                        return Ok(());
                    }
                    result = generate_plan_with_steering(
                        self.inner.client.as_ref(),
                        objective,
                        max_tasks,
                        &request.steering,
                    ) => options.source = PlanSource::Given(result?),
                }
            }
        }

        let mut config = config;
        // Every run of this manager shares one provider, so they share its
        // rate and in-flight budgets.
        config.shared_limits = Some(self.inner.limits.clone());
        // Per-run files: one trace path for every run would have each new
        // run truncate the trace of the ones still writing.
        if config.trace_path.is_some() {
            config.trace_path = Some(record.output_dir.join("trace.jsonl"));
        }
        config.journal_path = Some(record.output_dir.join("journal.jsonl"));
        options.scheduler = config;
        let mut hooks = ManagerHooks {
            record: &record,
            plan_only: request.plan_only,
        };
        options.hooks = Some(&mut hooks);
        let objective = request.objective.as_deref().unwrap_or_default();
        let outcome = match build(objective, Arc::clone(&self.inner.client), options).await {
            Ok(outcome) => outcome,
            Err(error) if error.downcast_ref::<Cancelled>().is_some() => {
                cancelled();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        // The hooks stopped after planning (cancelled, plan only).
        let Some(report) = outcome.report else {
            return Ok(());
        };
        save_json(&record.output_dir.join("report.json"), &report).await?;
        *record.report.write().expect("report lock") = Some(report.clone());
        if report.is_success() {
            record.set_state(RunStatus::Succeeded, None);
        } else if record.control.is_cancelled() || report.status_count(TaskStatus::Cancelled) > 0 {
            record.set_state(RunStatus::Cancelled, Some("run cancelled by user".into()));
        } else {
            record.set_state(
                RunStatus::Failed,
                Some("one or more tasks failed or were skipped".into()),
            );
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<RunSummary> {
        let records: Vec<Arc<RunRecord>> = self
            .inner
            .runs
            .read()
            .expect("run store lock")
            .values()
            .cloned()
            .collect();
        let mut summaries: Vec<RunSummary> =
            records.iter().map(|record| self.summary(record)).collect();
        summaries.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        summaries
    }

    pub fn get(&self, id: &str) -> Option<RunDetails> {
        self.record(id).map(|record| RunDetails {
            summary: self.summary(&record),
            steering: record.control.steering(),
            tasks: record.control.tasks(),
        })
    }

    pub fn plan(&self, id: &str) -> Option<Plan> {
        self.record(id)
            .and_then(|record| record.plan.read().expect("plan lock").clone())
    }

    pub fn report(&self, id: &str) -> Option<RunReport> {
        self.record(id)
            .and_then(|record| record.report.read().expect("report lock").clone())
    }

    pub fn steer(&self, id: &str, instruction: &str) -> Result<Option<RunDetails>> {
        let Some(record) = self.record(id) else {
            return Ok(None);
        };
        if record
            .state
            .read()
            .expect("run state lock")
            .status
            .is_terminal()
        {
            bail!("cannot steer a terminal run");
        }
        if record.control.is_cancelled() {
            bail!("cannot steer a run that is being cancelled");
        }
        record.control.steer(instruction.to_string())?;
        Ok(Some(RunDetails {
            summary: self.summary(&record),
            steering: record.control.steering(),
            tasks: record.control.tasks(),
        }))
    }

    pub fn cancel(&self, id: &str) -> Option<RunDetails> {
        let record = self.record(id)?;
        {
            let mut state = record.state.write().expect("run state lock");
            if !state.status.is_terminal() {
                state.status = RunStatus::Cancelling;
                state.updated_at_ms = unix_time_ms();
                record.control.cancel();
            }
        }
        Some(RunDetails {
            summary: self.summary(&record),
            steering: record.control.steering(),
            tasks: record.control.tasks(),
        })
    }

    fn record(&self, id: &str) -> Option<Arc<RunRecord>> {
        self.inner
            .runs
            .read()
            .expect("run store lock")
            .get(id)
            .cloned()
    }

    fn summary(&self, record: &RunRecord) -> RunSummary {
        let (status, updated_at_ms, error) = {
            let state = record.state.read().expect("run state lock");
            (state.status, state.updated_at_ms, state.error.clone())
        };
        let tasks = record.control.tasks();
        let steering_count = record.control.steering().len();
        let plan_task_count = record
            .plan
            .read()
            .expect("plan lock")
            .as_ref()
            .map(|plan| plan.tasks.len())
            .unwrap_or(0);
        let count = |wanted| tasks.values().filter(|task| task.status == wanted).count();
        RunSummary {
            id: record.id.clone(),
            objective: record.objective.clone(),
            status,
            plan_only: record.plan_only,
            created_at_ms: record.created_at_ms,
            updated_at_ms,
            task_count: tasks.len().max(plan_task_count),
            pending: count(TaskProgressStatus::Pending),
            running: count(TaskProgressStatus::Running),
            succeeded: count(TaskProgressStatus::Succeeded),
            failed: count(TaskProgressStatus::Failed),
            skipped: count(TaskProgressStatus::Skipped),
            cancelled: count(TaskProgressStatus::Cancelled),
            steering_count,
            has_plan: record.plan.read().expect("plan lock").is_some(),
            has_report: record.report.read().expect("report lock").is_some(),
            error,
            output_dir: record.output_dir.display().to_string(),
        }
    }
}

/// What `RunManager::execute` does after planning: record the plan, save
/// `plan.json`, and stop for a cancelled run or a plan-only request.
struct ManagerHooks<'a> {
    record: &'a Arc<RunRecord>,
    plan_only: bool,
}

#[crate::async_trait]
impl BuildHooks for ManagerHooks<'_> {
    async fn planned(
        &mut self,
        _: Option<&crate::architect::Manifest>,
        plan: &mut Plan,
        _: &mut SchedulerConfig,
        _: u64,
    ) -> Result<bool> {
        let record = self.record;
        debug!(run_id = %record.id, tasks = plan.tasks.len(), "plan candidate created");
        if record.control.is_cancelled() {
            record.set_state(
                RunStatus::Cancelled,
                Some("cancelled before execution".into()),
            );
            return Ok(false);
        }
        plan.validate()?;
        *record.plan.write().expect("plan lock") = Some(plan.clone());
        save_json(&record.output_dir.join("plan.json"), &*plan).await?;
        if self.plan_only {
            record.set_state(RunStatus::PlanReady, None);
            return Ok(false);
        }
        record.set_state(RunStatus::Running, None);
        Ok(true)
    }
}

async fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
    format!("run-{nanos:x}-{sequence:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architect::plan_project;
    use crate::client::MockClient;
    use crate::dag::Task;

    fn test_plan() -> Plan {
        Plan {
            cache_primes: Vec::new(),
            objective: "manager test".into(),
            tasks: vec![Task {
                id: "only".into(),
                title: "Only task".into(),
                prompt: "Complete the test".into(),
                deps: Vec::new(),
                max_tokens: 32,
                temperature: Some(0.0),
                require_closed_fences: false,
                validate_artifact: false,
                static_output: None,
                reasoning_effort: None,
                internal: false,
                prefix_split: None,
            }],
        }
    }

    #[tokio::test]
    async fn submits_and_collects_a_run() {
        let root = std::env::temp_dir().join(format!("slimtoken-manager-{}", new_run_id()));
        let manager = RunManager::new(
            Arc::new(MockClient::new(Duration::from_millis(1))),
            SchedulerConfig::default(),
            root.clone(),
            false,
        );
        let submitted = manager
            .submit(RunRequest {
                plan: Some(test_plan()),
                steering: vec!["Keep the output short".into()],
                ..RunRequest::default()
            })
            .await
            .unwrap();

        let mut details = manager.get(&submitted.id).unwrap();
        for _ in 0..100 {
            if details.summary.status.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            details = manager.get(&submitted.id).unwrap();
        }
        assert_eq!(details.summary.status, RunStatus::Succeeded);
        assert_eq!(details.summary.succeeded, 1);
        assert_eq!(details.steering, vec!["Keep the output short"]);
        assert!(manager.report(&submitted.id).is_some());
        assert!(Path::new(&details.summary.output_dir)
            .join("report.json")
            .exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Answers every export fragment with the files it lists, one export
    /// each, so architect planning runs without a model. Refuses the
    /// single-shot manifest prompt, so a path that drops `force_split` fails.
    struct FragmentEcho;

    #[async_trait::async_trait]
    impl ModelClient for FragmentEcho {
        async fn complete(
            &self,
            request: crate::client::CompletionRequest,
        ) -> Result<crate::client::Completion> {
            if request
                .user
                .contains("Design the file manifest for this objective")
            {
                bail!("FragmentEcho answers export fragments only");
            }
            let files: Vec<serde_json::Value> = request
                .user
                .lines()
                .filter_map(|line| line.strip_prefix("- `"))
                .filter_map(|rest| rest.split_once("`: "))
                .map(|(path, purpose)| {
                    let name = path.trim_end_matches(".py").replace('/', "_");
                    serde_json::json!({
                        "path": path,
                        "purpose": purpose,
                        "exports": [format!("def {name}() -> int")],
                    })
                })
                .collect();
            let text = serde_json::json!({"files": files, "test_files": [], "readme": false});
            Ok(crate::client::Completion {
                text: text.to_string(),
                provider: None,
                input_tokens: 10,
                output_tokens: 10,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".to_string()),
                usage_is_estimated: true,
                elapsed: Duration::ZERO,
                time_to_first_token: None,
            })
        }
    }

    /// Lists its modules (skeleton from the objective) and asks for enough
    /// tests to take the wave rather than one direct call.
    const LISTED_OBJECTIVE: &str = "Build a ledger. Modules: (1) ledger.py - core money \
        math for every entry; (2) store.py - JSON storage of all records; (3) cli.py - \
        argparse front end wiring everything. Include a pytest suite of at least 90 tests.";

    async fn planned(manager: &RunManager, request: RunRequest) -> Plan {
        let submitted = manager.submit(request).await.unwrap();
        for _ in 0..200 {
            if manager
                .get(&submitted.id)
                .unwrap()
                .summary
                .status
                .is_terminal()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let details = manager.get(&submitted.id).unwrap();
        assert_eq!(details.summary.status, RunStatus::PlanReady);
        manager.plan(&submitted.id).expect("plan ready")
    }

    #[tokio::test]
    async fn manager_and_cli_paths_give_the_same_plan() {
        let root = std::env::temp_dir().join(format!("slimtoken-manager-{}", new_run_id()));
        let manager = RunManager::new(
            Arc::new(FragmentEcho),
            SchedulerConfig::default(),
            root.clone(),
            false,
        );
        let options = PlanOptions {
            density: true,
            ..PlanOptions::default()
        };
        let from_manager = planned(
            &manager,
            RunRequest {
                objective: Some(LISTED_OBJECTIVE.into()),
                architect: true,
                plan_only: true,
                profile: Some("python".into()),
                plan_options: Some(options),
                ..RunRequest::default()
            },
        )
        .await;

        // The CLI path: plan_project with the same profile and options, and
        // the CLI's `--manifest` replay of the manifest it returns.
        let python = Profile::named("python");
        let (manifest, from_engine) = plan_project(
            &FragmentEcho,
            LISTED_OBJECTIVE,
            12,
            &[],
            &ArchitectOptions::default(),
            &options,
            python,
        )
        .await
        .unwrap();
        let replayed =
            crate::architect::plan_from_manifest(LISTED_OBJECTIVE, &manifest, 12, python, &options)
                .unwrap();
        let json = |plan: &Plan| serde_json::to_string(plan).unwrap();
        assert_eq!(json(&from_manager), json(&from_engine));
        assert_eq!(json(&from_manager), json(&replayed));
        assert_eq!(manifest.files.len(), 3);

        // No profile and no options: the plan is built as before.
        let legacy = planned(
            &manager,
            RunRequest {
                objective: Some(LISTED_OBJECTIVE.into()),
                architect: true,
                plan_only: true,
                ..RunRequest::default()
            },
        )
        .await;
        let before = generate_manifest_plan_with_options(
            &FragmentEcho,
            LISTED_OBJECTIVE,
            12,
            &[],
            true,
            None,
        )
        .await
        .unwrap();
        assert_eq!(json(&legacy), json(&before));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn plan_options_deserialize_with_defaults() {
        let request: RunRequest = serde_json::from_str(
            r#"{"objective":"x","architect":true,"profile":"nextjs","plan_options":{"density":true}}"#,
        )
        .unwrap();
        let options = request.plan_options.unwrap();
        assert!(options.density);
        assert_eq!(
            options,
            PlanOptions {
                density: true,
                ..PlanOptions::default()
            }
        );
        request.validate().unwrap();
        let unknown = RunRequest {
            objective: Some("x".into()),
            architect: true,
            profile: Some("cobol".into()),
            ..RunRequest::default()
        };
        assert!(unknown.validate().is_err());
        let not_architect = RunRequest {
            architect: false,
            ..request.clone()
        };
        assert!(not_architect.validate().is_err());
        let huge_quota = RunRequest {
            plan_options: Some(PlanOptions {
                quota_pct: u64::MAX,
                ..PlanOptions::default()
            }),
            ..request.clone()
        };
        assert!(huge_quota.validate().is_err());
        let many_shards = RunRequest {
            plan_options: Some(PlanOptions {
                min_test_shards: 10_000,
                ..PlanOptions::default()
            }),
            ..request.clone()
        };
        assert!(many_shards.validate().is_err());
        for (date, valid) in [
            ((2026, 11, 14), true),
            ((2026, 13, 1), false),
            ((2026, 0, 1), false),
            ((2026, 1, 32), false),
            ((2026, 1, 0), false),
            ((0, 1, 1), false),
            ((10_000, 1, 1), false),
        ] {
            let dated = RunRequest {
                plan_options: Some(PlanOptions {
                    today: Some(date),
                    ..PlanOptions::default()
                }),
                ..request.clone()
            };
            assert_eq!(dated.validate().is_ok(), valid, "{date:?}");
        }
    }

    #[test]
    fn rejects_ambiguous_requests() {
        let request = RunRequest {
            objective: Some("objective".into()),
            plan: Some(test_plan()),
            ..RunRequest::default()
        };
        assert!(request.validate().is_err());
    }
}
