//! One call from an objective to the files of a project: [`build`].
//!
//! [`build`] runs the steps that the CLI's `run` and [`RunManager`] run, in
//! this order:
//!
//! 1. plan: [`plan_project_with`] (or a plan that the host made, see
//!    [`PlanSource::Given`]);
//! 2. write the files: [`execute_plan_with_control`], then join a rendered
//!    style sheet that only imports its parts into one file
//!    ([`join_imported_stylesheets`](crate::stylesheet::join_imported_stylesheets));
//! 3. put them on disk: [`write_project`] (with [`BuildOptions::out_dir`]);
//! 4. check and repair them: [`check_and_repair_indexed`] (when a
//!    [`Checker`] is set). The files are written again after the repair, so
//!    the directory holds the files that were kept
//!    ([`BuildOptions::write_before_check`] off: only then).
//!
//! What a host does between the steps (save the plan, stop after planning,
//! a sink task, a check of its own) goes in [`BuildHooks`].
//!
//! [`BuildOptions::default`] plans and runs as the library did before this
//! module: no staging, no test-shard floor, the model sink. The measured
//! settings of a stack come from [`BuildOptions::for_stack`], and only for a
//! profile whose staging is measured (`Staging::measured`).
//!
//! ```no_run
//! use std::sync::Arc;
//! use slimtoken::pipeline::{build, BuildOptions};
//! use slimtoken::profile::NEXTJS;
//! use slimtoken::provider::DEEPSEEK_PRESET;
//!
//! async fn host(
//!     client: Arc<dyn slimtoken::ModelClient>,
//!     checker: &dyn slimtoken::check::Checker,
//! ) -> anyhow::Result<()> {
//!     let options = BuildOptions {
//!         out_dir: Some("app".into()),
//!         checker: Some(checker),
//!         ..BuildOptions::for_stack(&NEXTJS, Some(&DEEPSEEK_PRESET))
//!     };
//!     let outcome = build("Build a task tracker in Next.js", client, options).await?;
//!     println!("{} files written", outcome.files.len());
//!     Ok(())
//! }
//! ```
//!
//! [`RunManager`]: crate::runner::RunManager

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use crate::architect::{
    plan_project_with, ArchitectOptions, Manifest, ManifestSource, PlanOptions,
};
use crate::check::Checker;
use crate::client::ModelClient;
use crate::control::RunControl;
use crate::dag::Plan;
use crate::guard::CodeIndex;
use crate::inwave::{execute_with_in_wave_repair, InWave, InWaveOptions, InWaveReport};
use crate::profile::{Profile, RepairHooks};
use crate::project::{write_project, WrittenFile};
use crate::provider::ProviderPreset;
use crate::repair::{check_and_repair_seeded, RepairPolicy, RepairSeed, RepairSummary};
use crate::scheduler::{execute_plan_with_control, RunReport, SchedulerConfig};

/// Where [`build`] gets its plan.
#[derive(Debug, Default)]
pub enum PlanSource {
    /// The architect plans the objective ([`plan_project_with`] with
    /// [`BuildOptions::max_tasks`], `steering`, `architect`, `plan`,
    /// `profile` and `manifest`).
    #[default]
    Architect,
    /// A plan that the host made (a loaded `plan.json`, a direct or a mock
    /// plan, the general planner's).
    Given(Plan),
}

/// What the host makes the final document of a run from. This is advice
/// for the host: [`build`] does not read it, appends no sink task and makes
/// no final document. A host that wants the model sink appends it in
/// [`BuildHooks::planned`] (the CLI does, with `--sink model`), and a host
/// that gets [`Sink::Assemble`] joins the files itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sink {
    /// A model task that merges the files into one document.
    #[default]
    Model,
    /// The files joined in plan order, with no model call.
    Assemble,
}

/// The sink for an architect build with `profile`: [`Sink::Assemble`] when
/// its staging is measured (the staged wave writes project files, which a
/// merge would only copy), else [`Sink::Model`].
pub fn stack_sink(profile: &Profile) -> Sink {
    if profile.staging.measured {
        Sink::Assemble
    } else {
        Sink::Model
    }
}

/// Planning stopped because the run's [`RunControl`] was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled during planning")
    }
}

impl std::error::Error for Cancelled {}

/// What a host does between the steps of [`build`]. Every method has a
/// default that does nothing.
#[crate::async_trait]
pub trait BuildHooks: Send {
    /// After planning, before the wave. `manifest` is the planned manifest
    /// (`None` for a [`PlanSource::Given`] plan). The hook may change the
    /// plan (a sink task) and the scheduler config of the wave. `false`
    /// stops the build here: the outcome has no report.
    async fn planned(
        &mut self,
        manifest: Option<&Manifest>,
        plan: &mut Plan,
        scheduler: &mut SchedulerConfig,
        planning_ms: u64,
    ) -> Result<bool> {
        let _ = (manifest, plan, scheduler, planning_ms);
        Ok(true)
    }

    /// After the wave (and the join of a split style sheet), before the
    /// files are written and checked.
    /// `scheduler` is the config of the repair runs (no journal). `false`
    /// skips the check.
    async fn executed(
        &mut self,
        plan: &mut Plan,
        report: &mut RunReport,
        scheduler: &mut SchedulerConfig,
    ) -> Result<bool> {
        let _ = (plan, report, scheduler);
        Ok(true)
    }

    /// After the check and repair (or after the wave when no check ran),
    /// before the files are written to [`BuildOptions::out_dir`] (the last
    /// time). `repair` and `repair_error` are the step's result (see
    /// [`BuildOptions::keep_check_errors`]).
    async fn finished(
        &mut self,
        plan: &Plan,
        report: &RunReport,
        repair: Option<&RepairSummary>,
        repair_error: Option<&anyhow::Error>,
    ) -> Result<()> {
        let _ = (plan, report, repair, repair_error);
        Ok(())
    }

    /// A checker made in [`Self::planned`], asked for after
    /// [`Self::executed`]; it wins over [`BuildOptions::checker`].
    fn checker(&self) -> Option<&dyn Checker> {
        None
    }

    /// A code index for the repair guards, asked for like [`Self::checker`];
    /// it wins over [`BuildOptions::index`].
    fn index(&self) -> Option<&dyn CodeIndex> {
        None
    }

    /// The checker of the in-wave repair ([`BuildOptions::in_wave`] without
    /// a checker of its own), asked for after [`Self::planned`]; `None`
    /// takes [`Self::checker`], then [`BuildOptions::checker`], but only a
    /// checker of landed files ([`Checker::checks_landed_files`]): with none,
    /// the wave runs without in-wave repair (a warning says so).
    fn in_wave_checker(&self) -> Option<&dyn Checker> {
        None
    }
}

/// How [`build`] runs. The default plans with the architect and the library
/// defaults, writes no files to disk and runs no check.
pub struct BuildOptions<'a> {
    pub source: PlanSource,
    /// Most tasks the architect may plan.
    pub max_tasks: usize,
    pub steering: Vec<String>,
    pub architect: ArchitectOptions,
    pub plan: PlanOptions,
    /// Stack profile; `None` takes `architect.profile`, or detects it from
    /// the objective (for the repair hooks: from the plan's objective).
    pub profile: Option<&'static Profile>,
    /// A saved manifest and a pass over it (see [`ManifestSource`]).
    pub manifest: ManifestSource<'a, 'a>,
    /// The client of the planning calls; `None` plans with the wave's client.
    pub planner: Option<Arc<dyn ModelClient>>,
    /// The config of the wave and of the repair runs (these never write the
    /// journal).
    pub scheduler: SchedulerConfig,
    /// Cancels planning (with the error [`Cancelled`]) and the wave.
    pub control: RunControl,
    /// When planning started, for `planning_ms`; `None` is the call of
    /// [`build`].
    pub started: Option<Instant>,
    /// What the host makes the final document from (see [`Sink`]).
    pub sink: Sink,
    /// Write the project's files here ([`write_project`]).
    pub out_dir: Option<PathBuf>,
    /// With a checker: also write the wave's files before the check (so a
    /// check that fails with an error leaves them on disk); the kept files
    /// are always written after it. Off saves that write on the way to the
    /// first check (about 20 ms for a 49-file app on Windows).
    pub write_before_check: bool,
    /// Check and repair the files after the wave.
    pub checker: Option<&'a dyn Checker>,
    pub repair: RepairPolicy,
    /// The repair hooks; `None` takes the profile's `repair`.
    pub repair_hooks: Option<&'a RepairHooks>,
    /// Where the repair guards look up who uses what; `None` reads the
    /// sources.
    pub index: Option<&'a dyn CodeIndex>,
    /// An error of the check and repair step goes into
    /// [`BuildOutcome::repair_error`] and the build goes on with the wave's
    /// files; off, the build returns the error.
    pub keep_check_errors: bool,
    pub hooks: Option<&'a mut dyn BuildHooks>,
    /// Repair landed files during the wave's tail (`crate::inwave`), then
    /// check and repair the whole app as always. `None` (the default): off.
    /// Needs a checker.
    pub in_wave: Option<InWaveOptions<'a>>,
}

impl Default for BuildOptions<'_> {
    fn default() -> Self {
        Self {
            source: PlanSource::default(),
            max_tasks: 12,
            steering: Vec::new(),
            architect: ArchitectOptions::default(),
            plan: PlanOptions::default(),
            profile: None,
            manifest: ManifestSource::default(),
            planner: None,
            scheduler: SchedulerConfig::default(),
            control: RunControl::default(),
            started: None,
            sink: Sink::default(),
            out_dir: None,
            write_before_check: true,
            checker: None,
            repair: RepairPolicy::default(),
            repair_hooks: None,
            index: None,
            keep_check_errors: false,
            hooks: None,
            in_wave: None,
        }
    }
}

impl BuildOptions<'_> {
    /// The options for an architect build of `profile` with `preset`: its
    /// profile, the preset's scheduler and planning knobs, and
    /// [`Self::apply_stack`].
    pub fn for_stack(profile: &'static Profile, preset: Option<&ProviderPreset>) -> Self {
        let mut options = Self {
            profile: Some(profile),
            ..Self::default()
        };
        options.architect.profile = Some(profile);
        if let Some(preset) = preset {
            preset.apply_scheduler(&mut options.scheduler);
        }
        options.apply_stack(profile, preset);
        options
    }

    /// Stamp the measured settings of `profile` and `preset` over these
    /// options: the preset's architect knobs
    /// ([`ProviderPreset::apply_architect`]) and plan shape
    /// ([`ProviderPreset::apply_plan`]: staging, one owner for each type,
    /// the manifest passes and the test-shard floor, only for a profile
    /// whose staging is measured and only where the options still hold the
    /// library default), and the sink of [`stack_sink`] when the plan ends
    /// up staged. An unstaged plan (no preset, or a preset that is not
    /// DeepSeek) keeps [`Sink::Model`]: its wave writes fragments, not
    /// project files.
    pub fn apply_stack(&mut self, profile: &'static Profile, preset: Option<&ProviderPreset>) {
        if let Some(preset) = preset {
            preset.apply_architect(&mut self.architect);
            preset.apply_plan(&mut self.plan, profile);
        }
        self.sink = if self.plan.staged {
            stack_sink(profile)
        } else {
            Sink::Model
        };
    }
}

/// What [`build`] did.
#[derive(Debug)]
pub struct BuildOutcome {
    /// The plan as run (a repair round can add a file to it).
    pub plan: Plan,
    /// The manifest the architect planned (`None` for a given plan).
    pub manifest: Option<Manifest>,
    /// `None` when [`BuildHooks::planned`] stopped the build.
    pub report: Option<RunReport>,
    pub repair: Option<RepairSummary>,
    /// The error of the check and repair step, with
    /// [`BuildOptions::keep_check_errors`].
    pub repair_error: Option<anyhow::Error>,
    /// The files written to [`BuildOptions::out_dir`] (the last write).
    pub files: Vec<WrittenFile>,
    pub planning_ms: u64,
    /// What the in-wave repair did (`None` with [`BuildOptions::in_wave`]
    /// off).
    pub in_wave: Option<InWaveReport>,
}

/// Plan `objective`, write its files with `client`, put them on disk and
/// check and repair them (see the module docs and [`BuildOptions`]).
pub async fn build(
    objective: &str,
    client: Arc<dyn ModelClient>,
    options: BuildOptions<'_>,
) -> Result<BuildOutcome> {
    let BuildOptions {
        source,
        max_tasks,
        steering,
        architect,
        plan: plan_options,
        profile,
        manifest: manifest_source,
        planner,
        mut scheduler,
        control,
        started,
        sink: _,
        out_dir,
        write_before_check,
        checker,
        repair: policy,
        repair_hooks,
        index,
        keep_check_errors,
        mut hooks,
        in_wave,
    } = options;
    let started = started.unwrap_or_else(Instant::now);

    // 1. Plan.
    let (manifest, mut plan) = match source {
        PlanSource::Given(plan) => (None, plan),
        PlanSource::Architect => {
            let planner = planner.unwrap_or_else(|| client.clone());
            let planning = plan_project_with(
                planner.as_ref(),
                objective,
                max_tasks,
                &steering,
                &architect,
                &plan_options,
                profile,
                manifest_source,
            );
            let (manifest, plan) = tokio::select! {
                _ = control.cancelled() => return Err(Cancelled.into()),
                planned = planning => planned?,
            };
            (Some(manifest), plan)
        }
    };
    let planning_ms = started.elapsed().as_millis() as u64;
    let stopped = |plan: Plan, manifest: Option<Manifest>| BuildOutcome {
        plan,
        manifest,
        report: None,
        repair: None,
        repair_error: None,
        files: Vec::new(),
        planning_ms,
        in_wave: None,
    };
    if let Some(hooks) = hooks.as_deref_mut() {
        if !hooks
            .planned(manifest.as_ref(), &mut plan, &mut scheduler, planning_ms)
            .await?
        {
            return Ok(stopped(plan, manifest));
        }
    }

    // 2. The wave.
    // The planning timeline's end (`architect.plan_pipe` A/B): the wave's
    // first request opens right after this line.
    tracing::info!(
        planning_done_ms = planning_ms,
        wave_first_open_ms = started.elapsed().as_millis() as u64,
        plan_pipe = architect.plan_pipe,
        "planning timeline: wave starts"
    );
    let repair_hooks: &RepairHooks = repair_hooks.unwrap_or_else(|| {
        &profile
            .or(architect.profile)
            .unwrap_or_else(|| Profile::detect(&plan.objective))
            .repair
    });
    // `in_wave`: landed files are checked and repaired beside the wave.
    // Only a checker meant for an app that is not all there: the full gate's
    // whole-app rules would see files that have not landed yet.
    let wave_checker = in_wave.as_ref().and_then(|options| {
        let hooks_ref = hooks.as_deref();
        let found = options
            .checker
            .or_else(|| hooks_ref.and_then(|hooks| hooks.in_wave_checker()))
            .or_else(|| {
                hooks_ref
                    .and_then(|hooks| hooks.checker())
                    .filter(|checker| checker.checks_landed_files())
            })
            .or(checker.filter(|checker| checker.checks_landed_files()));
        if found.is_none() {
            tracing::warn!(
                "in-wave repair is off: no checker of landed files (InWaveOptions::checker, \
                 BuildHooks::in_wave_checker or a checker whose checks_landed_files is true)"
            );
        }
        found.map(|checker| (checker, options))
    });
    let mut in_wave_report = None;
    let mut seed = RepairSeed::default();
    let mut report = match wave_checker {
        Some((wave_checker, options)) => {
            let hooks_ref = hooks.as_deref();
            let in_wave = InWave {
                checker: wave_checker,
                mode: options.mode,
                policy: &policy,
                hooks: repair_hooks,
                index: hooks_ref.and_then(|hooks| hooks.index()).or(index),
                client: options.client.clone().unwrap_or_else(|| client.clone()),
                on_repaired: options.on_repaired.clone(),
            };
            let outcome = execute_with_in_wave_repair(
                &plan,
                client.clone(),
                scheduler.clone(),
                control,
                &in_wave,
            )
            .await?;
            in_wave_report = Some(outcome.in_wave);
            seed = outcome.seed;
            outcome.report
        }
        None => {
            execute_plan_with_control(&plan, client.clone(), scheduler.clone(), control).await?
        }
    };
    // The repair runs must not open the journal again: that truncates it,
    // and a later resume would find only repair prompts.
    scheduler.journal_path = None;
    // One style sheet: a rendered sheet that only imports its parts (the
    // CSS split) gets their text, before any check reads the files.
    crate::stylesheet::join_imported_stylesheets(&mut plan, &mut report);
    let check = match hooks.as_deref_mut() {
        Some(hooks) => {
            hooks
                .executed(&mut plan, &mut report, &mut scheduler)
                .await?
        }
        None => true,
    };

    let hooks_ref = hooks.as_deref();
    let checker = hooks_ref
        .and_then(|hooks| hooks.checker())
        .or(checker)
        .filter(|_| check);
    let index = hooks_ref.and_then(|hooks| hooks.index()).or(index);
    // No check after the wave: the in-wave repairs were never checked
    // against the whole app, so the wave's own files are kept.
    if checker.is_none() {
        let restored = seed.restore(&mut report);
        if restored > 0 {
            crate::stylesheet::join_imported_stylesheets(&mut plan, &mut report);
            tracing::info!(
                restored,
                "in-wave repair: no check after the wave, the wave's own files are kept"
            );
        }
    }

    // 3. The wave's files on disk, before a check (`write_before_check`).
    let mut files = match &out_dir {
        Some(dir) if write_before_check && checker.is_some() => write_project(&report, &plan, dir)?,
        _ => Vec::new(),
    };

    // 4. Check and repair.
    let mut repair = None;
    let mut repair_error = None;
    if let Some(checker) = checker {
        let repair_start = Instant::now();
        match check_and_repair_seeded(
            &mut plan,
            &mut report,
            client,
            scheduler,
            checker,
            policy,
            repair_hooks,
            index,
            &seed,
        )
        .await
        {
            Ok(summary) => {
                let after_wave_ms = repair_start.elapsed().as_millis() as u64;
                report.phases.reconcile_ms = Some(after_wave_ms);
                if let Some(in_wave) = &in_wave_report {
                    tracing::info!(
                        in_wave_repairs = in_wave.in_wave_repairs,
                        in_wave_ms = in_wave.in_wave_ms,
                        after_wave_ms,
                        "in-wave repair: the step after the wave"
                    );
                }
                repair = Some(summary);
            }
            Err(error) if keep_check_errors => repair_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    if let Some(hooks) = hooks {
        hooks
            .finished(&plan, &report, repair.as_ref(), repair_error.as_ref())
            .await?;
    }
    // The files that were kept.
    if let Some(dir) = &out_dir {
        files = write_project(&report, &plan, dir)?;
    }
    Ok(BuildOutcome {
        plan,
        manifest,
        report: Some(report),
        repair,
        repair_error,
        files,
        planning_ms,
        in_wave: in_wave_report,
    })
}
