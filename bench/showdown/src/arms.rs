//! The two live arms. Each arm gets the same objective, the same model and
//! the same streaming client. Each arm writes its files to its own directory.

use crate::events::{Arm, CheckState, Emitter, Ev, FileStat, Kind, Phase, PlannedFile};
use crate::extract;
use crate::repair;
use crate::tap::TapClient;
use anyhow::{Context, Result};
use slimtoken::architect::{
    apply_data_note, apply_manifest_passes, declare_signature_types, dedupe_type_owners,
    first_type_owner_wins, generate_manifest, plan_from_manifest, plan_project_with,
    stamp_relative_dates, stamp_today, types_files_own_types, ArchitectOptions, Manifest,
    ManifestFile, ManifestSource, PlanOptions,
};
use slimtoken::dag::Plan;
use slimtoken::profile::{Layer, Profile, Staging};
use slimtoken::provider::ProviderPreset;
use slimtoken::scheduler::{execute_plan, SchedulerConfig, TaskStatus};
use slimtoken::{CompletionRequest, ModelClient};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub struct LiveConfig {
    pub objective: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub preset: Option<ProviderPreset>,
    pub effort: Option<String>,
    pub max_tasks: usize,
    pub single_max_tokens: u32,
    pub manifest: Option<PathBuf>,
    pub out_dir: PathBuf,
    /// Gate command for SlimToken's check-and-repair step; `None` skips it.
    pub gate_cmd: Option<String>,
    /// Write the wave in stages (types, then lib, then the rest).
    pub staged: bool,
    /// Most repair rounds after the wave.
    pub repair_rounds: usize,
}

/// The format request added to the objective for the single-stream arm. The
/// benchmark needs it to split the one answer into files.
pub const SINGLE_FORMAT: &str = "Deliver every file in full in this one answer. For each file, \
write a heading line with the file path in backticks (### `path/to/file`), then one fenced code \
block with the complete file content. Do not leave out or shorten any file.";

fn write_files(dir: &Path, files: &[(String, String)]) -> Result<Vec<FileStat>> {
    let mut stats = Vec::with_capacity(files.len());
    for (path, content) in files {
        let safe = if repair::engcheck() {
            slimtoken::project::safe_path(path)
        } else {
            extract::safe_path(path)
        };
        let Some(path) = safe else {
            continue;
        };
        let target = dir.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&target, content)
            .with_context(|| format!("failed to write {}", target.display()))?;
        stats.push(FileStat {
            path,
            lines: content.lines().count(),
            bytes: content.len(),
        });
    }
    Ok(stats)
}

fn fail(arm: Arm, emitter: &Emitter, out_dir: &Path, error: &anyhow::Error) {
    emitter.emit(Ev::Phase {
        arm,
        phase: Phase::Failed,
    });
    emitter.emit(Ev::Final {
        arm,
        ok: false,
        files: Vec::new(),
        out_dir: out_dir.display().to_string(),
        note: Some(format!("{error:#}").chars().take(400).collect()),
    });
}

pub async fn run_single(config: Arc<LiveConfig>, emitter: Emitter) {
    let arm = Arm::Single;
    let out_dir = config.out_dir.join(arm.dir_name());
    emitter.emit(Ev::ArmStart { arm });
    if let Err(error) = single(&config, &emitter, &out_dir).await {
        fail(arm, &emitter, &out_dir, &error);
    }
}

async fn single(config: &LiveConfig, emitter: &Emitter, out_dir: &Path) -> Result<()> {
    let arm = Arm::Single;
    let client = TapClient::new(
        &config.base_url,
        &config.model,
        config.api_key.clone(),
        arm,
        emitter.clone(),
    )?;
    emitter.emit(Ev::Phase {
        arm,
        phase: Phase::Streaming,
    });
    let effort = config
        .effort
        .clone()
        .or_else(|| config.preset.map(|preset| preset.worker_effort.to_string()));
    let request = CompletionRequest {
        system: "You are an expert software engineer. You deliver complete, working code."
            .to_string(),
        shared_context: None,
        user: format!("{}\n\n{SINGLE_FORMAT}", config.objective),
        max_tokens: config.single_max_tokens,
        temperature: 0.2,
        reasoning_effort: effort,
        prompt_cache_key: None,
        allow_empty: false,
        response_format: None,
    };
    let done = client
        .complete_as(request, "single response".to_string(), Kind::Single)
        .await?;
    // The clock stops here: the answer is complete. Disk writes are not timed.
    emitter.emit(Ev::Phase {
        arm,
        phase: Phase::Done,
    });

    std::fs::create_dir_all(out_dir)?;
    std::fs::create_dir_all(config.out_dir.join("_meta"))?;
    std::fs::write(
        config.out_dir.join("_meta").join("single-answer.md"),
        &done.text,
    )?;
    let found = if repair::engcheck() {
        slimtoken::project::scan(&done.text)
    } else {
        extract::scan(&done.text)
    };
    let truncated = done.finish_reason.as_deref() == Some("length");
    let files: Vec<(String, String)> = found
        .into_iter()
        // An unclosed block is a file the model did not finish.
        .filter(|file| file.closed)
        .map(|file| (file.path, file.content))
        .collect();
    let stats = write_files(out_dir, &files)?;
    emitter.emit(Ev::Final {
        arm,
        ok: !truncated && !stats.is_empty(),
        files: stats,
        out_dir: out_dir.display().to_string(),
        note: truncated.then(|| {
            format!(
                "answer cut off at the {}-token output limit",
                config.single_max_tokens
            )
        }),
    });
    Ok(())
}

pub async fn run_slim(config: Arc<LiveConfig>, emitter: Emitter) {
    let arm = Arm::Slim;
    let out_dir = config.out_dir.join(arm.dir_name());
    emitter.emit(Ev::ArmStart { arm });
    if let Err(error) = slim(&config, &emitter, &out_dir).await {
        fail(arm, &emitter, &out_dir, &error);
    }
}

async fn slim(config: &LiveConfig, emitter: &Emitter, out_dir: &Path) -> Result<()> {
    let arm = Arm::Slim;
    let meta_dir = config.out_dir.join("_meta");
    std::fs::create_dir_all(&meta_dir)?;
    let client = Arc::new(slim_transport(
        TapClient::new(
            &config.base_url,
            &config.model,
            config.api_key.clone(),
            arm,
            emitter.clone(),
        )?,
        step_on,
    )?);
    if step_on("prewarm") {
        prewarm(client.clone(), emitter.clone(), arm);
    }

    // The engine's command-line defaults: adaptive width, repair error diet.
    // With `traceall` the trace keeps the wave and every repair round (each
    // opens with a `plan_start` line); the run starts it empty.
    let trace_path = meta_dir.join("slimtoken-trace.jsonl");
    let trace_append = step_on("traceall");
    if trace_append {
        let _ = std::fs::remove_file(&trace_path);
    }
    let mut scheduler = SchedulerConfig {
        adaptive: true,
        reasoning_effort: config.effort.clone(),
        repair_error_chars: Some(2_000),
        trace_path: Some(trace_path),
        trace_append,
        ..SchedulerConfig::default()
    };
    // One owner file for each shared type (the engine's
    // `--architect-type-ownership`): workers that each declare their own
    // copy of a type write files that do not compile together.
    let mut architect = ArchitectOptions {
        type_ownership: true,
        ..ArchitectOptions::default()
    };
    // For A/B tests of the plan: files for each export request.
    if let Some(files) = std::env::var("SHOWDOWN_FPF")
        .ok()
        .and_then(|text| text.parse().ok())
    {
        architect.files_per_fragment = files;
    }
    if let Some(preset) = &config.preset {
        preset.apply_scheduler(&mut scheduler);
        preset.apply_architect(&mut architect);
    }
    // `qcontract` (off unless `SHOWDOWN_ON` names it): the planner asks the
    // types file for the app contract and checks the merged manifest.
    architect.contract = step_on("qcontract");
    // `pagelocal` (off unless `SHOWDOWN_ON` names it): the planner writes a
    // component that only one page renders in that page's file.
    architect.page_local = step_on("pagelocal");
    // `qsemantic` (off unless `SHOWDOWN_ON` names it): the planner states
    // the conventions in the types file and checks the merged manifest.
    architect.semantics = step_on("qsemantic");
    // `typesfirst`, `planrefs`, `planreview` (off unless `SHOWDOWN_ON` names
    // them): the shared types planned before the other export fragments, the
    // deterministic dangling-reference check, and one whole-plan review call,
    // each feeding the plan's single repair round.
    architect.types_first = step_on("typesfirst");
    // `layered` and `corewhole` (off unless `SHOWDOWN_ON` names them): the
    // planner's third fragment round (types, then core, then the files that
    // use them, those with the planned core and every such file's job), and
    // with it the core planned in one fragment instead of pairs.
    architect.layered = step_on("layered");
    architect.core_whole = step_on("corewhole");
    architect.references = step_on("planrefs");
    architect.review = step_on("planreview");
    // `planpipe` (off unless `SHOWDOWN_ON` names it): export fragments start
    // from the streaming skeleton when their prompts are already settled.
    architect.plan_pipe = step_on("planpipe");
    // `typespaths` (off unless `SHOWDOWN_ON` names it): the skeleton lists
    // its paths first and the types fragment starts from them, beside the
    // rest of the skeleton (it turns the streamed skeleton on).
    if step_on("typespaths") {
        architect.types_from_paths = true;
        architect.plan_pipe = true;
    }
    // `skelcompact` (off unless `SHOWDOWN_ON` names it): the skeleton call
    // answers `path: purpose` lines instead of pretty-printed JSON.
    let skel_sink = step_on("skelcompact").then(slimtoken::skelcompact::SkelSink::default);
    architect.skel_compact = skel_sink
        .clone()
        .map(|sink| slimtoken::skelcompact::SkelCompact { sink: Some(sink) });
    // `splitpage` (off unless `SHOWDOWN_ON` names it; needs `pagelocal`):
    // the local components of the final plan's pages become part files
    // written beside their pages, so the longest page no longer ends the
    // wave alone.
    let split_sink = split_page_on(step_on).then(slimtoken::splitpage::SplitSink::default);
    architect.split_pages = split_sink
        .clone()
        .map(|sink| slimtoken::splitpage::SplitPages {
            sink: Some(sink),
            ..slimtoken::splitpage::SplitPages::default()
        });
    // `examples` (off unless `SHOWDOWN_ON` names it): the plan, the module
    // prompts and the test prompts share one worked example for each pure
    // calculation.
    if step_on("examples") {
        let base = architect
            .profile
            .unwrap_or_else(|| Profile::detect(&config.objective));
        architect.profile = Some(with_examples(base));
    }
    if config.staged {
        wide_start(&mut scheduler);
    }
    no_cut(&mut scheduler, step_on);

    // ---- Plan ---------------------------------------------------------
    emitter.emit(Ev::Phase {
        arm,
        phase: Phase::Planning,
    });
    let planning = Instant::now();
    let saved: Option<slimtoken::architect::Manifest> = match &config.manifest {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Some(
                serde_json::from_str(&text)
                    .with_context(|| format!("invalid manifest {}", path.display()))?,
            )
        }
        None => None,
    };
    let profile = slim_profile(
        architect
            .profile
            .unwrap_or_else(|| Profile::detect(&config.objective)),
        config.staged,
    );
    // One date for the whole run: the plan, its staging and `_meta`.
    let date = today();
    // `engpipe`: the same steps through the engine's `pipeline::build`.
    if engpipe() {
        let model: Arc<dyn ModelClient> = client.clone();
        let run = SlimRun {
            config,
            emitter,
            meta_dir: &meta_dir,
            client,
            profile,
            date,
            planning,
            split: split_sink,
            skel: skel_sink,
        };
        return slim_pipeline(run, model, out_dir, scheduler, architect, saved)
            .await
            .map(|_| ());
    }
    // `specwave` (off unless `SHOWDOWN_ON` names it): the Types, Core and
    // Data files and the wiring start on the manifest as it stands before
    // the contract repair's answers (see `EarlyWave`).
    let spec =
        step_on("specwave") && config.staged && saved.is_none() && architect.split_pages.is_none();
    let early_manifest = spec.then(|| {
        let (sink, receiver) = slimtoken::architect::EarlyManifest::channel();
        architect.early_manifest = Some(sink);
        receiver
    });
    let run = SlimRun {
        config,
        emitter,
        meta_dir: &meta_dir,
        client: client.clone(),
        profile,
        date,
        planning,
        split: split_sink,
        skel: skel_sink,
    };
    // `pagefork` (off unless `SHOWDOWN_ON` names it): each page's local
    // components are written beside its main component.
    let fork = step_on("pagefork").then(|| Arc::new(crate::tap::PageForkTap::new(client.clone())));
    let model: Arc<dyn ModelClient> = match &fork {
        Some(fork) => fork.clone(),
        None => client.clone(),
    };
    let planned = plan_slim(
        client.as_ref(),
        config,
        &architect,
        saved,
        step_on("engplan"),
        profile,
        date,
    );
    let (manifest, mut plan, early) = match early_manifest {
        None => {
            let (manifest, plan) = planned.await?;
            (manifest, plan, None)
        }
        Some(receiver) => {
            tokio::pin!(planned);
            let first = tokio::select! {
                biased;
                done = &mut planned => Err(done),
                early = receiver => Ok(early),
            };
            match first {
                // No repair round: the plan is the final one.
                Err(done) => {
                    let (manifest, plan) = done?;
                    (manifest, plan, None)
                }
                Ok(early) => {
                    let early = match early {
                        Ok(manifest) => {
                            EarlyWave::start(&run, &architect, manifest, &model, &scheduler).await?
                        }
                        Err(_) => None,
                    };
                    let (manifest, plan) = planned.await?;
                    (manifest, plan, early)
                }
            }
        }
    };
    let planning_ms = run.finish_plan(&manifest, &mut plan)?;
    if let Some(fork) = &fork {
        fork.set_manifest(&manifest);
    }

    // ---- Wave ---------------------------------------------------------
    let (sidecar, indexio, reused, free) = match early {
        Some(early) => {
            let feeds = (early.sidecar.clone(), early.indexio.clone());
            let (reused, free) = early
                .settle(&run, &manifest, &mut plan, &model, &scheduler)
                .await?;
            (feeds.0, feeds.1, reused, free)
        }
        None => {
            let (sidecar, indexio) = run.start_wave(&plan);
            (sidecar, indexio, Vec::new(), None)
        }
    };
    // `inwave` (off unless `SHOWDOWN_ON` names it): landed files whose
    // imports have landed are checked by the persistent checker and
    // repaired while the wave's tail streams.
    let in_wave_sidecar = sidecar
        .clone()
        .filter(|_| in_wave_on(step_on) && config.gate_cmd.is_some());
    let (mut report, in_wave, seed) = match in_wave_sidecar {
        Some(sidecar) => {
            let checker = crate::pchk::LandedChecker(sidecar.clone());
            let policy = repair::engine_policy(config.repair_rounds, step_on);
            let index: Option<Arc<dyn slimtoken::guard::CodeIndex>> = indexio
                .clone()
                .map(|index| index as Arc<dyn slimtoken::guard::CodeIndex>);
            let in_wave = slimtoken::inwave::InWave {
                checker: &checker,
                mode: slimtoken::check::CheckMode::Fast,
                policy: &policy,
                hooks: &repair::BENCH_HOOKS,
                index: index.as_deref(),
                client: Arc::new(crate::tap::InWaveRepairTap(client.clone())),
                on_repaired: Some(in_wave_feed(sidecar)),
            };
            // `specwave`: the early tasks that nothing waits for run on
            // (the files kept from the early start are written files of
            // the plan: checked with the others, repaired after the wave).
            let rest = free.as_ref().map(|free| free.rest(&plan));
            let mut outcome = slimtoken::inwave::execute_with_in_wave_repair(
                rest.as_ref().unwrap_or(&plan),
                model.clone(),
                scheduler.clone(),
                slimtoken::RunControl::default(),
                &in_wave,
            )
            .await?;
            if let Some(free) = free {
                free.join(&plan, &mut outcome.report, &model, &scheduler)
                    .await?;
            }
            (outcome.report, Some(outcome.in_wave), outcome.seed)
        }
        None => {
            // `specwave`: the early tasks that nothing waits for run on.
            let mut report = match &free {
                Some(free) => {
                    execute_plan(&free.rest(&plan), model.clone(), scheduler.clone()).await?
                }
                None => execute_plan(&plan, model.clone(), scheduler.clone()).await?,
            };
            if let Some(free) = free {
                free.join(&plan, &mut report, &model, &scheduler).await?;
            }
            (report, None, slimtoken::repair::RepairSeed::default())
        }
    };
    // `specwave`: the files written early are the model's again (a repair
    // may change them), with their tokens.
    restore_reused(&mut plan, &mut report, reused);
    emit_plan_timeline(emitter, arm, &client, planning_ms);
    emit_skel_compact(emitter, arm, &client, run.skel.as_ref());
    emit_split_pages(emitter, arm, run.split.as_ref(), &report);
    if let Some(fork) = &fork {
        fork.end_wave();
        let stats = fork.stats();
        emitter.emit(Ev::Tool {
            arm,
            tool: "pagefork".to_string(),
            op: "summary".to_string(),
            ms: 0,
            critical_ms: 0,
            files: stats.pages as _,
            hits: stats.joined as _,
            note: format!(
                "pages={} forked={} parts={} joined={} failed={} dropped={} imports={}",
                stats.pages,
                stats.forked,
                stats.parts,
                stats.joined,
                stats.failed,
                stats.dropped,
                stats.imports
            ),
        });
    }
    let failed = run.end_wave(&report);
    // `inwave` with no check after the wave: its repairs were never checked
    // against the whole app, so the wave's own files are kept.
    if config.gate_cmd.is_none() || failed != 0 {
        seed.restore(&mut report);
    }
    // One style sheet, as `pipeline::build` delivers it: the parts of the
    // CSS split are joined into the sheet that imported them.
    slimtoken::stylesheet::join_imported_stylesheets(&mut plan, &mut report);

    // ---- Check and repair ----------------------------------------------
    // Type-check and test the assembled files, then repair the files that
    // the gate names (see `repair.rs`). This time is on the clock.
    if let Some(gate) = config.gate_cmd.as_deref().filter(|_| failed == 0) {
        let started = run.start_verify();
        let index: Option<Arc<dyn slimtoken::guard::CodeIndex>> = indexio
            .clone()
            .map(|index| index as Arc<dyn slimtoken::guard::CodeIndex>);
        let outcome = repair::run_seeded(
            &mut plan,
            &mut report,
            model,
            scheduler,
            gate,
            config.repair_rounds,
            sidecar,
            index,
            &seed,
        )
        .await;
        let after_wave_ms = started.elapsed().as_millis() as u64;
        run.record_repair(started, indexio.as_deref(), outcome)?;
        emit_in_wave(emitter, arm, in_wave.as_ref(), after_wave_ms);
    }
    emitter.emit(Ev::Phase {
        arm,
        phase: Phase::Done,
    });

    std::fs::create_dir_all(out_dir)?;
    let mut files = Vec::new();
    for task in plan.tasks.iter().filter(|task| !task.internal) {
        let Some(record) = report.tasks.get(&task.id) else {
            continue;
        };
        if record.status != TaskStatus::Succeeded {
            continue;
        }
        if let Some(output) = record
            .output
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            files.push((task.title.clone(), file_text(&task.title, output)));
        }
    }
    let stats = write_files(out_dir, &files)?;
    run.finish(out_dir, &report, stats, failed, planning_ms)
}

/// True when the `slim` arm runs through the engine's `pipeline::build`
/// (`engpipe`, off unless `SHOWDOWN_ON` names it). It needs the engine's
/// planning and repair loop (`engplan`, `engrepair`) and the engine's file
/// helpers (`engcheck`: the pipeline writes the files with
/// `project::write_project`); with any of them off the arm runs its own
/// steps as before.
fn engpipe() -> bool {
    engpipe_with(step_on)
}

/// `engpipe` with the switches read from `on`.
fn engpipe_with(on: impl Fn(&str) -> bool) -> bool {
    on("engpipe") && on("engplan") && on("engrepair") && on("engcheck")
}

/// `specwave`: the tasks of `plan` that can start before the Consumers: the
/// Types, Data and Core files, the rendered files, the internal tasks (the
/// wiring) and every task with no prerequisite (the style sheet parts), each
/// only if its prerequisites are among them.
fn early_tasks(plan: &Plan, staging: &Staging) -> Vec<slimtoken::dag::Task> {
    let mut keep: std::collections::BTreeSet<&str> = plan
        .tasks
        .iter()
        .filter(|task| {
            task.internal
                || task.static_output.is_some()
                || task.deps.is_empty()
                || matches!(
                    staging.layer_of(&task.title),
                    Some(Layer::Types | Layer::Data | Layer::Core)
                )
        })
        .map(|task| task.id.as_str())
        .collect();
    loop {
        let before = keep.len();
        for task in &plan.tasks {
            if task.deps.iter().any(|dep| !keep.contains(dep.as_str())) {
                keep.remove(task.id.as_str());
            }
        }
        if keep.len() == before {
            break;
        }
    }
    plan.tasks
        .iter()
        .filter(|task| keep.contains(task.id.as_str()))
        .cloned()
        .collect()
}

/// `specwave`: the ids of the written tasks of `tasks` that no task of
/// `plan` waits for and that wait for nothing themselves (the seed data):
/// the rest of the wave need not wait for them.
fn free_tasks(tasks: &[slimtoken::dag::Task], plan: &Plan) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| {
            task.static_output.is_none()
                && !task.internal
                && task.deps.is_empty()
                && !plan.tasks.iter().any(|other| other.deps.contains(&task.id))
        })
        .map(|task| task.id.clone())
        .collect()
}

/// The name that an export line of a types file declares: the first word
/// after its keywords.
fn declared_type(line: &str) -> Option<&str> {
    line.split("//")
        .next()?
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .find(|word| {
            !word.is_empty()
                && ![
                    "export",
                    "type",
                    "interface",
                    "const",
                    "function",
                    "enum",
                    "class",
                    "default",
                    "async",
                    "declare",
                    "abstract",
                ]
                .contains(word)
        })
}

fn has_word(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + name.len()..].chars().next().is_some_and(word)
    })
}

/// `specwave`: the type names that the contract repair changed between
/// `early` and `last`: the names of the export lines of the Types files that
/// are in one manifest only, and of every line that names one of those (a
/// type built on a changed type has changed). A line whose declaration is
/// the same and whose comment alone changed is not a changed type: the
/// second list has the clauses of its comment (between `;`) that one
/// manifest only has, and they concern the files that they name. (Helpdesk:
/// each run's repair added "firstResponseAt is set by addMessage
/// (lib/messages.ts) ..." to the comment of `Status`, and all 18 early
/// files were written again.) `None` when a Types file came, went or has
/// another purpose: then every type counts as changed.
fn changed_types(
    early: &Manifest,
    last: &Manifest,
    staging: &Staging,
) -> Option<(Vec<String>, Vec<String>)> {
    let code = |line: &str| line.split("//").next().unwrap_or("").trim().to_string();
    let clauses = |line: &str| -> Vec<String> {
        line.split_once("//")
            .map(|(_, comment)| comment.split(';').map(|c| c.trim().to_string()).collect())
            .unwrap_or_default()
    };
    let types = |manifest: &Manifest| -> Vec<ManifestFile> {
        manifest
            .files
            .iter()
            .filter(|file| staging.layer_of(&file.path) == Some(Layer::Types))
            .cloned()
            .collect()
    };
    let (before, after) = (types(early), types(last));
    if before.len() != after.len() {
        return None;
    }
    let mut changed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut lines: Vec<&String> = Vec::new();
    for (old, new) in before.iter().zip(&after) {
        if old.path != new.path || old.purpose != new.purpose || old.uses != new.uses {
            return None;
        }
        for line in old.exports.iter().chain(&new.exports) {
            if old.exports.contains(line) && new.exports.contains(line) {
                lines.push(line);
            } else {
                let name = declared_type(line)?;
                // The one line of the other manifest that declares the
                // same name with the same code: only the comment changed.
                let other = if old.exports.contains(line) {
                    &new.exports
                } else {
                    &old.exports
                };
                let twins: Vec<&String> = other
                    .iter()
                    .filter(|twin| declared_type(twin) == Some(name))
                    .collect();
                if let [twin] = twins[..] {
                    if code(twin) == code(line) {
                        let theirs = clauses(twin);
                        notes.extend(
                            clauses(line)
                                .into_iter()
                                .filter(|clause| !theirs.contains(clause)),
                        );
                        continue;
                    }
                }
                if !changed.iter().any(|known| known == name) {
                    changed.push(name.to_string());
                }
            }
        }
    }
    // A line that both manifests have and that names a changed type.
    loop {
        let before = changed.len();
        for line in &lines {
            let Some(name) = declared_type(line) else {
                continue;
            };
            if !changed.iter().any(|known| known == name)
                && changed.iter().any(|other| has_word(line, other))
            {
                changed.push(name.to_string());
            }
        }
        if changed.len() == before {
            return Some((changed, notes));
        }
    }
}

/// `specwave`: whether the file `path` is planned the same in `early` and in
/// `last`: its own entry, every file that its entry names (its `uses` and
/// its purpose), and the types: no type that the repair changed
/// ([`changed_types`]) is named in its entry. (All the Types files had to be
/// the same at first; a repair that only added `Settings` and gave
/// `AppState` its fields then threw away all 22 early files, 18 of which
/// name neither.)
fn same_contract(path: &str, early: &Manifest, last: &Manifest, staging: &Staging) -> bool {
    let entry = |manifest: &Manifest, path: &str| {
        manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .and_then(|file| serde_json::to_string(file).ok())
    };
    let Some(own) = last.files.iter().find(|file| file.path == path) else {
        return false;
    };
    let types_same = match changed_types(early, last, staging) {
        None => false,
        Some((changed, notes)) => {
            let text = serde_json::to_string(own).unwrap_or_default();
            !changed.iter().any(|name| has_word(&text, name))
                && !notes.iter().any(|clause| clause.contains(path))
        }
    };
    entry(early, path) == entry(last, path)
        && types_same
        && last
            .files
            .iter()
            .filter(|file| {
                file.path != path
                    && (own.purpose.contains(&file.path)
                        || own.uses.iter().any(|used| used.contains(&file.path)))
            })
            .all(|file| entry(early, &file.path) == entry(last, &file.path))
}

/// `specwave`: give the tasks written early back to the model (a repair may
/// change them) and count their tokens in the wave's report.
fn restore_reused(
    plan: &mut Plan,
    report: &mut slimtoken::RunReport,
    reused: Vec<slimtoken::scheduler::TaskRecord>,
) {
    for early in reused {
        if let Some(task) = plan.tasks.iter_mut().find(|task| task.id == early.id) {
            task.static_output = None;
        }
        report.total_input_tokens += early.input_tokens;
        report.total_output_tokens += early.output_tokens;
        if let Some(record) = report.tasks.get_mut(&early.id) {
            record.attempts = early.attempts;
            record.duration_ms = early.duration_ms;
            record.input_tokens = early.input_tokens;
            record.output_tokens = early.output_tokens;
            record.cached_input_tokens = early.cached_input_tokens;
        }
    }
}

/// `specwave`: the early tasks that nothing waits for, still running when
/// the final plan starts ([`EarlyWave::settle`]).
struct FreeRun {
    ids: Vec<String>,
    run: tokio::task::JoinHandle<Result<slimtoken::RunReport>>,
}

impl FreeRun {
    /// `plan` without the tasks of this run: the wave to execute beside it.
    fn rest(&self, plan: &Plan) -> Plan {
        Plan {
            objective: plan.objective.clone(),
            tasks: plan
                .tasks
                .iter()
                .filter(|task| !self.ids.contains(&task.id))
                .cloned()
                .collect(),
            cache_primes: plan.cache_primes.clone(),
        }
    }

    /// The end of the run: its records join the wave's `report`. A task
    /// that failed is written once more.
    async fn join(
        self,
        plan: &Plan,
        report: &mut slimtoken::RunReport,
        model: &Arc<dyn ModelClient>,
        scheduler: &SchedulerConfig,
    ) -> Result<()> {
        let mut early = self.run.await.context("the early wave stopped")??;
        let failed: Vec<slimtoken::dag::Task> = plan
            .tasks
            .iter()
            .filter(|task| {
                self.ids.contains(&task.id)
                    && early
                        .tasks
                        .get(&task.id)
                        .is_none_or(|record| record.status != TaskStatus::Succeeded)
            })
            .cloned()
            .collect();
        if !failed.is_empty() {
            let again = Plan {
                objective: plan.objective.clone(),
                tasks: failed,
                cache_primes: Vec::new(),
            };
            let again = execute_plan(&again, model.clone(), scheduler.clone()).await?;
            early.tasks.extend(again.tasks);
        }
        for id in &self.ids {
            if let Some(record) = early.tasks.remove(id) {
                report.total_input_tokens += record.input_tokens;
                report.total_output_tokens += record.output_tokens;
                report.task_count += 1;
                report.tasks.insert(id.clone(), record);
            }
        }
        Ok(())
    }
}

/// `specwave`: the early part of the wave, started on the manifest that the
/// contract repair's model round starts from. The round takes 1.5-5 s and
/// changes a few files; the Consumers wait for the Core in any case. When
/// the final plan is there ([`EarlyWave::settle`]), a file whose contract the
/// repair left as it was keeps its early text, and the others are written
/// again at once, so the Core never lands later than without the switch.
struct EarlyWave {
    /// The early manifest after the manifest passes, and its early tasks.
    manifest: Manifest,
    tasks: Vec<slimtoken::dag::Task>,
    /// The tasks that others wait for, and those that nothing waits for.
    run: tokio::task::JoinHandle<Result<slimtoken::RunReport>>,
    free: Option<FreeRun>,
    started: Instant,
    sidecar: Option<Arc<crate::pchk::Sidecar>>,
    indexio: Option<Arc<crate::codeindex::Indexio>>,
}

impl EarlyWave {
    /// Plan `manifest` as the run plans its final one, start the wave's
    /// feeds and run the early tasks. `None`: nothing to start early.
    async fn start(
        run: &SlimRun<'_>,
        architect: &ArchitectOptions,
        manifest: Manifest,
        model: &Arc<dyn ModelClient>,
        scheduler: &SchedulerConfig,
    ) -> Result<Option<Self>> {
        let config = run.config;
        let (manifest, mut plan) = plan_slim(
            run.client.as_ref(),
            config,
            architect,
            Some(manifest),
            step_on("engplan"),
            run.profile,
            run.date,
        )
        .await?;
        run.stage(&manifest, &mut plan)?;
        // For work on the switch: what the early wave was planned from.
        let _ = std::fs::write(
            run.meta_dir.join("slimtoken-early-manifest.json"),
            serde_json::to_string_pretty(&manifest)?,
        );
        let staging = &run
            .profile
            .unwrap_or_else(|| Profile::detect(&config.objective))
            .staging;
        let tasks = early_tasks(&plan, staging);
        if !tasks.iter().any(|task| task.static_output.is_none()) {
            return Ok(None);
        }
        let (sidecar, indexio) = run.start_wave(&plan);
        let free_ids = free_tasks(&tasks, &plan);
        let spawn = |tasks: Vec<slimtoken::dag::Task>| {
            let sub = Plan {
                objective: plan.objective.clone(),
                tasks,
                cache_primes: Vec::new(),
            };
            let (model, scheduler) = (model.clone(), scheduler.clone());
            tokio::spawn(async move { execute_plan(&sub, model, scheduler).await })
        };
        let started = Instant::now();
        let needed = spawn(
            tasks
                .iter()
                .filter(|task| !free_ids.contains(&task.id))
                .cloned()
                .collect(),
        );
        let free = (!free_ids.is_empty()).then(|| FreeRun {
            run: spawn(
                tasks
                    .iter()
                    .filter(|task| free_ids.contains(&task.id))
                    .cloned()
                    .collect(),
            ),
            ids: free_ids,
        });
        Ok(Some(Self {
            manifest,
            tasks,
            run: needed,
            free,
            started,
            sidecar,
            indexio,
        }))
    }

    /// The final `plan` (staged) is there: keep the early text of each task
    /// whose request is the same or whose file is planned the same
    /// ([`same_contract`]), write the other early tasks again now, beside
    /// the early run, and put both in `plan` as written files. Returns the
    /// records of those tasks ([`restore_reused`] after the wave) and the
    /// kept tasks that nothing waits for, still running ([`FreeRun`]). With
    /// nothing to keep the early run is dropped and the plan runs as it is.
    async fn settle(
        self,
        run: &SlimRun<'_>,
        manifest: &Manifest,
        plan: &mut Plan,
        model: &Arc<dyn ModelClient>,
        scheduler: &SchedulerConfig,
    ) -> Result<(Vec<slimtoken::scheduler::TaskRecord>, Option<FreeRun>)> {
        let config = run.config;
        let staging = &run
            .profile
            .unwrap_or_else(|| Profile::detect(&config.objective))
            .staging;
        let lead_ms = self.started.elapsed().as_millis() as u64;
        let last = early_tasks(plan, staging);
        let keeps = |task: &slimtoken::dag::Task| {
            task.static_output.is_none()
                && self.tasks.iter().any(|early| {
                    early.id == task.id
                        && early.title == task.title
                        && early.static_output.is_none()
                        && early.deps == task.deps
                        && (early.prompt == task.prompt
                            || (!task.internal
                                && same_contract(&task.title, &self.manifest, manifest, staging)))
                })
        };
        let kept: Vec<String> = last
            .iter()
            .filter(|task| keeps(task))
            .map(|task| task.id.clone())
            .collect();
        // The kept tasks that nothing waits for, in the final plan too.
        let still_free = free_tasks(&last, plan);
        let free = self.free.and_then(|mut free| {
            free.ids
                .retain(|id| kept.contains(id) && still_free.contains(id));
            if free.ids.is_empty() {
                free.run.abort();
                None
            } else {
                Some(free)
            }
        });
        let is_free = |id: &String| free.as_ref().is_some_and(|free| free.ids.contains(id));
        // The tasks that the Consumers wait for: kept, or written again.
        let kept_needed: Vec<String> = kept.iter().filter(|id| !is_free(id)).cloned().collect();
        // A task to write again that nothing waits for (the seed data) stays
        // in the plan: the wave writes it beside the Consumers. (Written
        // here, the Consumers waited for it: 37.2 s against 28-29 s.)
        let again: Vec<slimtoken::dag::Task> = last
            .iter()
            .filter(|task| !kept.contains(&task.id) && !still_free.contains(&task.id))
            .cloned()
            .collect();
        let again_free = last
            .iter()
            .filter(|task| !kept.contains(&task.id) && still_free.contains(&task.id))
            .count();
        let again_ids: Vec<String> = again
            .iter()
            .filter(|task| task.static_output.is_none())
            .map(|task| task.id.clone())
            .collect();
        // A task to write again that needs a kept one cannot start before
        // the early run ends: no gain, so the plan runs as it is.
        let blocked = again
            .iter()
            .any(|task| task.deps.iter().any(|dep| kept.contains(dep)));
        let note = |op: &str, kept: usize, again: usize| {
            run.emitter.emit(Ev::Tool {
                arm: Arm::Slim,
                tool: "specwave".to_string(),
                op: op.to_string(),
                ms: lead_ms,
                critical_ms: 0,
                files: kept as _,
                hits: again as _,
                note: format!(
                    "early start {lead_ms} ms before the final plan; kept {kept}, written again {again}"
                ),
            });
        };
        // The rendered files of the final plan replace the early plan's.
        for task in plan.tasks.iter().filter(|task| !task.internal) {
            if let Some(output) = &task.static_output {
                run.client.land(&task.title, output);
            }
        }
        if kept.is_empty() || blocked {
            self.run.abort();
            if let Some(free) = free {
                free.run.abort();
            }
            note("miss", 0, again_ids.len() + again_free);
            return Ok((Vec::new(), None));
        }
        let delta = Plan {
            objective: plan.objective.clone(),
            tasks: again,
            cache_primes: Vec::new(),
        };
        let rewrite = async {
            if again_ids.is_empty() {
                Ok(None)
            } else {
                execute_plan(&delta, model.clone(), scheduler.clone())
                    .await
                    .map(Some)
            }
        };
        let (early, rewritten) = tokio::join!(self.run, rewrite);
        let early = early.context("the early wave stopped")??;
        let rewritten = rewritten?;
        let mut records = Vec::new();
        for task in plan.tasks.iter_mut() {
            let record = if kept_needed.contains(&task.id) {
                early.tasks.get(&task.id)
            } else if again_ids.contains(&task.id) {
                rewritten
                    .as_ref()
                    .and_then(|report| report.tasks.get(&task.id))
            } else {
                None
            };
            let Some(record) = record.filter(|record| record.status == TaskStatus::Succeeded)
            else {
                continue;
            };
            let Some(output) = &record.output else {
                continue;
            };
            // The last word on the feeds (an early text of a file written
            // again may have landed after the new one).
            if !task.internal {
                run.client.land(&task.title, output);
            }
            task.static_output = Some(output.to_string());
            records.push(record.clone());
        }
        note(
            if again_ids.is_empty() && again_free == 0 {
                "hit"
            } else {
                "part"
            },
            kept.len(),
            again_ids.len() + again_free,
        );
        Ok((records, free))
    }
}

/// What the steps of one `slim` run share: the parts around the engine's
/// calls (events, `_meta`, the feeds of the persistent checker and of
/// indexio), so that the bench path and `engpipe` run the same code there.
struct SlimRun<'a> {
    config: &'a LiveConfig,
    emitter: &'a Emitter,
    meta_dir: &'a Path,
    client: Arc<TapClient>,
    /// `slim_profile`'s profile (`None`: detected from the objective).
    profile: Option<&'static Profile>,
    date: (i32, u32, u32),
    /// When planning started.
    planning: Instant,
    /// `splitpage`: the report of the page split (`None`: switch off).
    split: Option<slimtoken::splitpage::SplitSink>,
    /// `skelcompact`: the report of the compact skeleton (`None`: switch off).
    skel: Option<slimtoken::skelcompact::SkelSink>,
}

impl SlimRun<'_> {
    /// The bench's staging steps on a planned `manifest`, and the plan's
    /// check.
    fn stage(&self, manifest: &Manifest, plan: &mut Plan) -> Result<()> {
        let config = self.config;
        if config.staged {
            finish_staging(
                plan,
                &config.objective,
                manifest,
                config.max_tasks,
                self.profile,
                &slim_plan_options_with(config.staged, step_on, self.date),
                step_on,
            )?;
            render_css_with(plan, manifest, step_on);
        }
        plan.validate().context("invalid execution plan")
    }

    /// The end of planning for the planned `manifest` (after its manifest
    /// passes): the bench's staging steps, the check, `_meta` and the
    /// `Planned` event. Returns the planning time.
    fn finish_plan(&self, manifest: &Manifest, plan: &mut Plan) -> Result<u64> {
        let config = self.config;
        self.stage(manifest, plan)?;
        let planning_ms = self.planning.elapsed().as_millis() as u64;
        self.client.mark_planning_done();
        std::fs::write(
            self.meta_dir.join("slimtoken-manifest.json"),
            serde_json::to_string_pretty(manifest)?,
        )?;
        // `qcontract`: the contract problems left in the planned manifest
        // (after the fragment repair and the manifest passes).
        if step_on("qcontract") {
            let profile = self
                .profile
                .unwrap_or_else(|| Profile::detect(&config.objective));
            let problems = slimtoken::contract::contract_problems(manifest, profile);
            std::fs::write(
                self.meta_dir.join("slimtoken-contract.json"),
                serde_json::to_string_pretty(&problems)?,
            )?;
        }
        // `qsemantic`: the convention problems left, and the conventions
        // block that the module workers got.
        if step_on("qsemantic") {
            let profile = self
                .profile
                .unwrap_or_else(|| Profile::detect(&config.objective));
            let report = serde_json::json!({
                "problems": slimtoken::semantics::semantic_problems(manifest, profile),
                "conventions": slimtoken::semantics::conventions(manifest, profile),
            });
            std::fs::write(
                self.meta_dir.join("slimtoken-semantics.json"),
                serde_json::to_string_pretty(&report)?,
            )?;
        }
        self.emitter.emit(Ev::Planned {
            arm: Arm::Slim,
            files: plan
                .tasks
                .iter()
                .filter(|task| !task.internal)
                .map(|task| PlannedFile {
                    path: task.title.clone(),
                    rendered: task.static_output.is_some(),
                })
                .collect(),
        });
        Ok(planning_ms)
    }

    /// Start the wave: `pchk` gives the persistent checker the rendered
    /// files now and each written file as its request closes (see
    /// `pchk.rs`); `SHOWDOWN_INDEX=indexio` has the repair guards look up
    /// who uses what in indexio, which indexes the files as they land (see
    /// `codeindex.rs`).
    fn start_wave(
        &self,
        plan: &Plan,
    ) -> (
        Option<Arc<crate::pchk::Sidecar>>,
        Option<Arc<crate::codeindex::Indexio>>,
    ) {
        let (config, client) = (self.config, &self.client);
        // `shellfix`: count this run's pages only (its wave checks too).
        crate::shellfix::take();
        let sidecar = config
            .gate_cmd
            .as_deref()
            .filter(|_| crate::pchk::on() && repair::engrepair())
            .and_then(|gate| crate::pchk::Sidecar::for_gate(gate).ok());
        if let Some(sidecar) = &sidecar {
            let files = plan.tasks.iter().filter(|task| !task.internal);
            sidecar.expect_tests(
                files
                    .clone()
                    .filter(|task| repair::is_test_file(&task.title))
                    .map(|task| task.title.clone()),
            );
            for task in files {
                if let Some(output) = &task.static_output {
                    sidecar.land(&task.title, output);
                }
            }
            client.set_feed(Some(sidecar.clone()));
        }
        let indexio = config
            .gate_cmd
            .as_ref()
            .filter(|_| crate::codeindex::wanted() && repair::engrepair() && crate::guard::on())
            .and_then(|_| {
                crate::codeindex::Indexio::start(
                    &self.meta_dir.join("indexio"),
                    self.emitter.clone(),
                    Arm::Slim,
                )
                .map_err(|error| {
                    let _ = std::fs::write(
                        self.meta_dir.join("indexio-error.txt"),
                        format!("{error:#}"),
                    );
                })
                .ok()
            });
        if let Some(index) = &indexio {
            for task in plan.tasks.iter().filter(|task| !task.internal) {
                if let Some(output) = &task.static_output {
                    index.land(&task.title, &file_text(&task.title, output));
                }
            }
            client.set_index_feed(Some(index.clone()));
        }
        client.set_in_wave(true);
        self.emitter.emit(Ev::Phase {
            arm: Arm::Slim,
            phase: Phase::Wave,
        });
        (sidecar, indexio)
    }

    /// The end of the wave: the feeds stop. Returns the failed tasks.
    fn end_wave(&self, report: &slimtoken::RunReport) -> usize {
        let failed = report.tasks.len() - report.status_count(TaskStatus::Succeeded);
        self.client.set_feed(None);
        self.client.set_index_feed(None);
        emit_no_cut(self.emitter, Arm::Slim, &self.client);
        emit_speed_race(self.emitter, Arm::Slim, &self.client);
        failed
    }

    /// The start of the check and repair step (on the clock).
    fn start_verify(&self) -> Instant {
        self.emitter.emit(Ev::Phase {
            arm: Arm::Slim,
            phase: Phase::Verify,
        });
        let started = Instant::now();
        crate::guard::take_layout_fixes();
        started
    }

    /// The end of the check and repair step that started at `started`: stop
    /// indexio, send the guard and `Verified` events, and save the summary
    /// (with what the persistent checker and indexio did) or the error of a
    /// gate that cannot run (that is not a failed build: the wave's files
    /// stay).
    fn record_repair(
        &self,
        started: Instant,
        indexio: Option<&crate::codeindex::Indexio>,
        outcome: Result<(repair::RepairSummary, Option<crate::pchk::PchkReport>)>,
    ) -> Result<()> {
        let (arm, emitter, meta_dir) = (Arm::Slim, self.emitter, self.meta_dir);
        let layout_fixes = crate::guard::take_layout_fixes();
        let shell_fixes = crate::shellfix::take();
        if crate::shellfix::on() {
            emit_shell_fix(emitter, arm, &shell_fixes);
        }
        if let Some(index) = indexio {
            index.stop();
        }
        match outcome {
            Ok((summary, pchk)) => {
                // `guard`: what the guards did, for the pane and the results.
                if let Some(guard) = &summary.guard {
                    emitter.emit(Ev::Tool {
                        arm,
                        tool: "guard".to_string(),
                        op: "summary".to_string(),
                        ms: 0,
                        critical_ms: 0,
                        files: guard.root_fixes + layout_fixes,
                        hits: guard.lock_breaks,
                        note: format!(
                            "{} index · {} locks · {} lock breaks · {} root fixes · {} root repairs · {} held · {} root waits · {} knock-on skipped · {} root-only rounds · {} layout fixes before the first check",
                            guard.index,
                            guard.lock_blocks,
                            guard.lock_breaks,
                            guard.root_fixes,
                            guard.root_repairs,
                            guard.held,
                            guard.root_waits,
                            guard.knock_on_skipped,
                            guard.root_only_rounds,
                            layout_fixes
                        ),
                    });
                }
                emitter.emit(Ev::Verified {
                    arm,
                    before_failed: summary.before.failed,
                    after_failed: summary.after.failed,
                    tests_passed: summary.after.tests_passed,
                    repaired: summary.repaired.len(),
                    seconds: started.elapsed().as_secs_f64(),
                });
                // `pchk`: what the persistent checker did, beside the rounds.
                let mut saved = serde_json::to_value(&summary)?;
                if let Some(pchk) = pchk {
                    saved["pchk"] = serde_json::to_value(pchk)?;
                }
                if summary.guard.is_some() {
                    saved["layout_fixes"] = layout_fixes.into();
                }
                if crate::shellfix::on() {
                    saved["shellfix"] = serde_json::json!({
                        "shellfix_applied": shell_fixes.0.len(),
                        "fixed": shell_fixes.0,
                        "left": shell_fixes.1,
                    });
                }
                if let Some(index) = indexio {
                    let stats = index.stats();
                    saved["indexio"] = serde_json::to_value(&stats)?;
                    saved["indexio"]["degraded"] = stats.degraded().into();
                }
                let _ = std::fs::write(
                    meta_dir.join("slimtoken-repair.json"),
                    serde_json::to_string_pretty(&saved)?,
                );
            }
            Err(error) => {
                let _ = std::fs::write(
                    meta_dir.join("slimtoken-repair-error.txt"),
                    format!("{error:#}"),
                );
            }
        }
        Ok(())
    }

    /// The end of the run, after the files are on disk (`stats`): the
    /// report in `_meta` and the `Final` event.
    fn finish(
        &self,
        out_dir: &Path,
        report: &slimtoken::RunReport,
        stats: Vec<FileStat>,
        failed: usize,
        planning_ms: u64,
    ) -> Result<()> {
        std::fs::write(
            self.meta_dir.join("slimtoken-report.json"),
            serde_json::to_string_pretty(report)?,
        )?;
        self.emitter.emit(Ev::Final {
            arm: Arm::Slim,
            ok: failed == 0 && !stats.is_empty(),
            files: stats,
            out_dir: out_dir.display().to_string(),
            note: Some(match failed {
                0 => format!("planned in {:.1}s", planning_ms as f64 / 1000.0),
                count => format!("{count} task(s) failed"),
            }),
        });
        Ok(())
    }
}

/// The `slim` arm through `pipeline::build` (`engpipe`), with `model` for
/// the model calls (the run's `TapClient`; `run.client` only takes the
/// feeds): the engine plans
/// (`plan_project_with` with the bench's manifest steps as its manifest
/// pass, the options and profile of the `eng*` switches), runs the wave,
/// checks and repairs with the bench gate (`repair::GateChecker`, the
/// bench's repair hooks and policy, the guards' index) and writes the files.
/// The bench parts run in the hooks ([`PipeHooks`]) at the same points as on
/// the bench path, so the events, `_meta` and the clock are the same; the
/// files go to disk after `Done`, as there
/// (`BuildOptions::write_before_check` off). Returns the plan as run.
async fn slim_pipeline(
    run: SlimRun<'_>,
    model: Arc<dyn ModelClient>,
    out_dir: &Path,
    scheduler: SchedulerConfig,
    architect: ArchitectOptions,
    saved: Option<Manifest>,
) -> Result<Plan> {
    let config = run.config;
    let date = run.date;
    let base = architect
        .profile
        .unwrap_or_else(|| Profile::detect(&config.objective));
    let options = slim_plan_options_with(config.staged, step_on, date);
    let mut prepare = |manifest: &mut Manifest| -> Result<()> {
        prepare_manifest_with(manifest, config.staged, base, step_on, date);
        Ok(())
    };
    let profile = run.profile;
    // `inwave`: the persistent checker of the landed files comes with the
    // wave's start (`PipeHooks::planned`).
    let in_wave_slot: Arc<std::sync::OnceLock<Arc<crate::pchk::Sidecar>>> = Arc::default();
    let in_wave = in_wave_on(step_on).then(|| {
        let slot = in_wave_slot.clone();
        let feed: slimtoken::inwave::RepairedHook = Arc::new(move |title, output| {
            if let Some(sidecar) = slot.get() {
                sidecar.land(title, output);
            }
        });
        let repairs: Arc<dyn ModelClient> =
            Arc::new(crate::tap::InWaveRepairTap(run.client.clone()));
        slimtoken::inwave::InWaveOptions {
            client: Some(repairs),
            on_repaired: Some(feed),
            ..slimtoken::inwave::InWaveOptions::default()
        }
    });
    let mut hooks = PipeHooks {
        run,
        base,
        options,
        planning_ms: 0,
        failed: 0,
        verify: None,
        sidecar: None,
        indexio: None,
        gate: None,
        in_wave_slot,
        landed: None,
    };
    let build_options = slimtoken::pipeline::BuildOptions {
        max_tasks: config.max_tasks,
        architect,
        plan: options,
        profile,
        manifest: ManifestSource {
            saved,
            prepare: Some(&mut prepare),
        },
        scheduler,
        started: Some(hooks.run.planning),
        out_dir: Some(out_dir.to_path_buf()),
        write_before_check: false,
        repair: repair::engine_policy(config.repair_rounds, step_on),
        repair_hooks: Some(&repair::BENCH_HOOKS),
        keep_check_errors: true,
        hooks: Some(&mut hooks),
        in_wave,
        ..slimtoken::pipeline::BuildOptions::default()
    };
    let outcome = slimtoken::pipeline::build(&config.objective, model, build_options).await?;
    std::fs::create_dir_all(out_dir)?;
    let report = outcome
        .report
        .context("the pipeline stopped after planning")?;
    if hooks.verify.is_some() {
        emit_in_wave(
            hooks.run.emitter,
            Arm::Slim,
            outcome.in_wave.as_ref(),
            report.phases.reconcile_ms.unwrap_or(0),
        );
    }
    let stats = outcome
        .files
        .into_iter()
        .map(|file| FileStat {
            path: file.path,
            lines: file.lines,
            bytes: file.bytes,
        })
        .collect();
    hooks
        .run
        .finish(out_dir, &report, stats, hooks.failed, hooks.planning_ms)?;
    Ok(outcome.plan)
}

/// The bench parts of an `engpipe` run, between the engine's steps.
struct PipeHooks<'a> {
    run: SlimRun<'a>,
    /// The built-in profile (not `slim_profile`'s edit of it).
    base: &'static Profile,
    options: PlanOptions,
    planning_ms: u64,
    failed: usize,
    /// When the check and repair step started (only when it runs).
    verify: Option<Instant>,
    sidecar: Option<Arc<crate::pchk::Sidecar>>,
    indexio: Option<Arc<crate::codeindex::Indexio>>,
    /// The bench gate as the engine's checker, with the persistent checker.
    gate: Option<repair::GateChecker>,
    /// `inwave`: the persistent checker for the repaired files' feed, and
    /// as the checker of the landed files.
    in_wave_slot: Arc<std::sync::OnceLock<Arc<crate::pchk::Sidecar>>>,
    landed: Option<crate::pchk::LandedChecker>,
}

#[slimtoken::async_trait]
impl slimtoken::pipeline::BuildHooks for PipeHooks<'_> {
    async fn planned(
        &mut self,
        manifest: Option<&Manifest>,
        plan: &mut Plan,
        _: &mut SchedulerConfig,
        _: u64,
    ) -> Result<bool> {
        // The manifest as `plan_slim` returns it: with the engine's
        // manifest passes (the plan ran them on its own copy).
        let mut manifest = manifest.context("the pipeline plans a manifest")?.clone();
        apply_manifest_passes(
            &mut manifest,
            self.run.profile.unwrap_or(self.base),
            &self.options,
        );
        self.planning_ms = self.run.finish_plan(&manifest, plan)?;
        let (sidecar, indexio) = self.run.start_wave(plan);
        self.gate = self
            .run
            .config
            .gate_cmd
            .as_deref()
            .map(|gate| repair::GateChecker::with_sidecar(gate, sidecar.clone()));
        if let Some(sidecar) = sidecar.as_ref().filter(|_| in_wave_on(step_on)) {
            let _ = self.in_wave_slot.set(sidecar.clone());
            self.landed = Some(crate::pchk::LandedChecker(sidecar.clone()));
        }
        self.sidecar = sidecar;
        self.indexio = indexio;
        Ok(true)
    }

    async fn executed(
        &mut self,
        _: &mut Plan,
        report: &mut slimtoken::RunReport,
        _: &mut SchedulerConfig,
    ) -> Result<bool> {
        // The planning timeline, at the same point as on the bench path.
        emit_plan_timeline(
            self.run.emitter,
            Arm::Slim,
            &self.run.client,
            self.planning_ms,
        );
        emit_skel_compact(
            self.run.emitter,
            Arm::Slim,
            &self.run.client,
            self.run.skel.as_ref(),
        );
        emit_split_pages(self.run.emitter, Arm::Slim, self.run.split.as_ref(), report);
        self.failed = self.run.end_wave(report);
        let check = self.gate.is_some() && self.failed == 0;
        if check {
            self.verify = Some(self.run.start_verify());
        }
        Ok(check)
    }

    fn checker(&self) -> Option<&dyn slimtoken::check::Checker> {
        self.gate
            .as_ref()
            .map(|gate| gate as &dyn slimtoken::check::Checker)
    }

    fn index(&self) -> Option<&dyn slimtoken::guard::CodeIndex> {
        self.indexio
            .as_deref()
            .map(|index| index as &dyn slimtoken::guard::CodeIndex)
    }

    /// `inwave`: the persistent checker; with none running, a checker that
    /// gives no result (never the command-line gate, whose rules read the
    /// whole app).
    fn in_wave_checker(&self) -> Option<&dyn slimtoken::check::Checker> {
        Some(match &self.landed {
            Some(landed) => landed as &dyn slimtoken::check::Checker,
            None => &crate::pchk::NoLandedChecker,
        })
    }

    async fn finished(
        &mut self,
        _: &Plan,
        _: &slimtoken::RunReport,
        summary: Option<&repair::RepairSummary>,
        error: Option<&anyhow::Error>,
    ) -> Result<()> {
        if let Some(started) = self.verify {
            let pchk = self.gate.as_ref().and_then(|gate| gate.pchk_report());
            let outcome = match (summary, error) {
                (Some(summary), _) => Ok((summary.clone(), pchk)),
                (None, Some(error)) => Err(anyhow::anyhow!("{error:#}")),
                (None, None) => Err(anyhow::anyhow!("the check did not run")),
            };
            self.run
                .record_repair(started, self.indexio.as_deref(), outcome)?;
        }
        self.run.emitter.emit(Ev::Phase {
            arm: Arm::Slim,
            phase: Phase::Done,
        });
        Ok(())
    }
}

/// Run the check-and-repair step again on the SlimToken app of a finished
/// run (`run_dir`), from the files on disk. For work on the repair step: it
/// needs no new wave. The files on disk do not change.
pub async fn rerepair(
    config: &LiveConfig,
    emitter: &Emitter,
    run_dir: &Path,
    arm: &str,
) -> Result<repair::RepairSummary> {
    let client = Arc::new(TapClient::new(
        &config.base_url,
        &config.model,
        config.api_key.clone(),
        Arm::Slim,
        emitter.clone(),
    )?);
    let mut scheduler = SchedulerConfig {
        adaptive: true,
        reasoning_effort: config.effort.clone(),
        repair_error_chars: Some(2_000),
        ..SchedulerConfig::default()
    };
    if let Some(preset) = &config.preset {
        preset.apply_scheduler(&mut scheduler);
    }
    let app = run_dir.join(arm);
    let text = if arm == "slimtoken" {
        let path = run_dir.join("_meta").join("slimtoken-manifest.json");
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        // The app of the single stream has no plan: its files are the plan.
        // This gives the single stream the same repair step as SlimToken.
        manifest_of_files(&app)?
    };
    let manifest: slimtoken::architect::Manifest =
        serde_json::from_str(&text).context("invalid manifest")?;
    let options = rerepair_plan_options(&manifest, &app, config.staged);
    let profile = slim_profile(Profile::detect(&config.objective), config.staged);
    let mut plan = plan_from_manifest(
        &config.objective,
        &manifest,
        config.max_tasks.max(200),
        profile,
        &options,
    )?;
    if config.staged {
        finish_staging(
            &mut plan,
            &config.objective,
            &manifest,
            config.max_tasks.max(200),
            profile,
            &options,
            step_on,
        )?;
    }
    // The manifest still lists the parts of a joined style sheet; the app on
    // disk holds the one sheet, so it is repaired as the whole-sheet task.
    slimtoken::stylesheet::join_planned_stylesheets(&mut plan, |path| app.join(path).is_file());
    plan.tasks
        .retain(|task| !task.internal && app.join(&task.title).is_file());
    // The files on disk enter the report as rendered tasks: no model call.
    let mut on_disk = plan.clone();
    for task in &mut plan.tasks {
        task.deps.clear();
    }
    for task in &mut on_disk.tasks {
        task.deps.clear();
        let content = std::fs::read_to_string(app.join(&task.title))?;
        task.static_output = Some(format!("```\n{}\n```", content.trim_end()));
    }
    let model: Arc<dyn ModelClient> = client.clone();
    let mut report = execute_plan(&on_disk, model.clone(), scheduler.clone()).await?;
    let gate = config
        .gate_cmd
        .as_deref()
        .context("no gate for this objective")?;
    let summary = repair::run(
        &mut plan,
        &mut report,
        model,
        scheduler,
        gate,
        config.repair_rounds,
    )
    .await?;
    // The repaired app, beside the run's own (which stays as it was), so
    // the repair's changes can be read.
    let repaired = run_dir.join(format!("{arm}-rerepaired"));
    slimtoken::project::write_project(&report, &plan, &repaired)?;
    Ok(summary)
}

/// A manifest (JSON text) with one entry for each source file under `app`.
fn manifest_of_files(app: &Path) -> Result<String> {
    let mut files = Vec::new();
    let mut dirs = vec![app.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            if path.is_dir() {
                if !matches!(name.as_str(), "node_modules" | ".next" | "_meta" | ".git") {
                    dirs.push(path);
                }
                continue;
            }
            let is_source = [".ts", ".tsx", ".css", ".mjs", ".js", ".json"]
                .iter()
                .any(|end| name.ends_with(end));
            if !is_source || name == "package-lock.json" || name == "next-env.d.ts" {
                continue;
            }
            let rel = path
                .strip_prefix(app)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let entry = serde_json::json!({
                "path": rel,
                "purpose": "A file of the app as it is now.",
                "exports": [],
            });
            // Tests stay in `files`: the engine plans `test_files` as new
            // shards with other paths, and these files exist already.
            files.push(entry);
        }
    }
    Ok(serde_json::json!({ "files": files, "test_files": [], "readme": false }).to_string())
}

/// The text of the file `path` in a worker's answer, read the same way as the
/// gate reads it (see `repair::assemble_where`; with `engcheck` on, the
/// engine's `project::file_text`, the same code): if the answer has more than one
/// code block, the block that the gate reads is the file. What goes to disk is thus
/// exactly what the gate checked.
pub(crate) fn file_text(path: &str, output: &str) -> String {
    if repair::engcheck() {
        return slimtoken::project::file_text(path, output);
    }
    let doc = format!(
        "## {path}

{}
",
        output.trim()
    );
    extract::scan(&doc)
        .into_iter()
        .find(|file| file.path == path && file.closed)
        .map(|file| file.content)
        .unwrap_or_else(|| extract::strip_fence(output))
}

/// The manifest and plan of the `slim` arm. With `engine` (switch
/// `engplan`) it plans through the engine's `plan_project_with`, with the
/// bench steps as its manifest pass; without, through the bench's own calls.
/// Both run the bench steps after the manifest is written (or replayed from
/// `saved`) and before it is planned. `profile` is `slim_profile`'s (`None`:
/// detected from the objective). The manifest returned is the one planned:
/// with `engtidy` on, the engine's manifest passes run on it too (the plan
/// runs them on its own copy), so `_meta` keeps the tidied manifest as
/// before and `rerepair` keeps the run's date.
async fn plan_slim(
    client: &dyn ModelClient,
    config: &LiveConfig,
    architect: &ArchitectOptions,
    saved: Option<slimtoken::architect::Manifest>,
    engine: bool,
    profile: Option<&'static Profile>,
    date: (i32, u32, u32),
) -> Result<(slimtoken::architect::Manifest, slimtoken::dag::Plan)> {
    // The profile as built in (`profile` may be `slim_profile`'s edit of it).
    let base = architect
        .profile
        .unwrap_or_else(|| Profile::detect(&config.objective));
    let options = slim_plan_options_with(config.staged, step_on, date);
    let planned = |(mut manifest, plan): (Manifest, Plan)| -> (Manifest, Plan) {
        apply_manifest_passes(&mut manifest, profile.unwrap_or(base), &options);
        (manifest, plan)
    };
    if engine {
        let mut prepare = |manifest: &mut slimtoken::architect::Manifest| -> Result<()> {
            prepare_manifest_with(manifest, config.staged, base, step_on, date);
            Ok(())
        };
        return plan_project_with(
            client,
            &config.objective,
            config.max_tasks,
            &[],
            architect,
            &options,
            profile,
            ManifestSource {
                saved,
                prepare: Some(&mut prepare),
            },
        )
        .await
        .map(planned);
    }
    let mut manifest = match saved {
        Some(manifest) => manifest,
        None => {
            generate_manifest(client, &config.objective, config.max_tasks, &[], architect).await?
        }
    };
    prepare_manifest_with(&mut manifest, config.staged, base, step_on, date);
    // `splitpage`: as `plan_project_with` does on the engine path.
    if let Some(split) = &architect.split_pages {
        slimtoken::splitpage::split_pages(
            client,
            &config.objective,
            &mut manifest,
            config.max_tasks,
            profile.unwrap_or(base),
            &options,
            split,
            architect,
        )
        .await;
    }
    let plan = plan_from_manifest(
        &config.objective,
        &manifest,
        config.max_tasks,
        profile,
        &options,
    )?;
    Ok(planned((manifest, plan)))
}

/// The plan options of the `slim` arm. A staged run asks the engine for
/// `TEST_FILES` test shards (`shards`), unless `engshards` is off: the
/// bench then pads the manifest's test files itself (`spread_tests_with`).
/// `engshards` is temporary: it and `spread_tests_with` go once the A/B runs
/// show the two give the same results. A staged run also asks the engine
/// to stage the plan (`engstage`; see `slim_profile`), to split the style
/// sheet (`csssplit`, with `engcss` on) and, with `engtidy`
/// on, to run its manifest passes in place of the bench's `tidy_manifest`
/// and seed notes (see `prepare_manifest_with`). The switches are read
/// from `on` (a run passes `step_on`); `today` is the run's date, read once
/// per run (`today()`), so its plan, staging and `_meta` manifest agree.
fn slim_plan_options_with(
    staged: bool,
    on: impl Fn(&str) -> bool,
    today: (i32, u32, u32),
) -> PlanOptions {
    let mut options = PlanOptions::default();
    if staged && on("shards") && on("engshards") {
        options.min_test_shards = TEST_FILES;
    }
    options.staged = staged && on("engstage");
    options.split = staged && on("csssplit") && on("engcss");
    let tidy = staged && on("engtidy");
    // One of `onetype` and `oneowner` alone runs in `prepare_manifest_with`.
    options.one_type_owner = tidy && on("onetype") && on("oneowner");
    options.tidy = tidy;
    options.declare_types = tidy && on("typedecl");
    // With the seed out of the Data layer of the staged profile (`seed`
    // off), the seed notes run in `prepare_manifest_with` on the built-in
    // one.
    let seed_moved = options.staged && !on("seed");
    if tidy && on("seedcompact") && on("seeddates") && !seed_moved {
        // `qrules`: dates relative to the day the data loads, not this day.
        if on("qrules") {
            options.relative_dates = true;
        } else {
            options.today = Some(today);
        }
    }
    // The quality steps (off unless `SHOWDOWN_ON` names them).
    options.quality_rules = on("qrules");
    options.objective_pages = on("qobjective");
    options.align_literals = on("qliteral");
    // Quality step Q2 (off unless `SHOWDOWN_ON` names them).
    options.contract = on("qcontract");
    options.wave_wiring = on("qwiring");
    // `testsee` (off unless `SHOWDOWN_ON` names it): a test file waits for the
    // modules it covers and sees their code (`PlanOptions::tests_see_code`).
    options.tests_see_code = on("testsee");
    options.wiring_rules = on("qwiring");
    options.consumers_see_css = staged && on("qcss");
    options.deps_first = on("prefix");
    // Pages own their features (off unless `SHOWDOWN_ON` names it).
    options.page_local = on("pagelocal");
    // The conventions block (off unless `SHOWDOWN_ON` names it).
    options.semantics = on("qsemantic");
    // Pages do not draw the layout's frame again (off unless named).
    options.shell_rule = on("shellrule");
    // Core and the seed data start with the wave, on the planned types (off
    // unless named; only where the types file is planned first).
    options.types_from_plan = staged && on("typesplan") && on("typesfirst");
    options
}

/// Today's date (UTC) for the seed data (`seeddates`).
fn today() -> (i32, u32, u32) {
    let (year, month, day, ..) = crate::utc_now();
    (year as i32, month, day)
}

/// The plan options for `rerepair` on a saved manifest. A run that wrote
/// the manifest's own test files (made `--flat`, or with `shards` or
/// `engshards` off) has no engine shards on disk: it is planned without
/// the floor, so its test files are checked again (still staged by the
/// engine with `engstage` on). The manifest passes stay off whatever
/// `engtidy` says: the `slimtoken` arm's saved manifest is the one it
/// planned (already tidied, with its run's date), and the single stream's
/// manifest of files must reach the repair prompts as it is.
fn rerepair_plan_options(
    manifest: &slimtoken::architect::Manifest,
    app: &Path,
    staged: bool,
) -> PlanOptions {
    rerepair_plan_options_with(manifest, app, staged, step_on)
}

/// `rerepair_plan_options` with the switches read from `on`.
fn rerepair_plan_options_with(
    manifest: &slimtoken::architect::Manifest,
    app: &Path,
    staged: bool,
    on: impl Fn(&str) -> bool,
) -> PlanOptions {
    // The date does not matter: `today` stays off.
    let options = PlanOptions {
        one_type_owner: false,
        tidy: false,
        declare_types: false,
        today: None,
        split: false,
        relative_dates: false,
        align_literals: false,
        // The quality steps describe a plan's mechanisms (and rewrite its
        // lines): the saved manifest, the single stream's above all, reaches
        // the repair prompts as it is.
        quality_rules: false,
        objective_pages: false,
        // Q2 too: the saved outputs are replayed as they are (no wiring task
        // to replay, no new deps or prompt layout).
        contract: false,
        wave_wiring: false,
        wiring_rules: false,
        consumers_see_css: false,
        deps_first: false,
        // The saved manifest is the folded one: its pages already hold their
        // local components (and their note keeps them at 8192 max_tokens,
        // in the repair plan too).
        page_local: false,
        // The conventions block describes the plan's mechanisms: the saved
        // manifest reaches the repair prompts as it is.
        semantics: false,
        shell_rule: false,
        // A repair plan writes against the files on disk.
        types_from_plan: false,
        ..slim_plan_options_with(staged, on, (1970, 1, 1))
    };
    let wrote_own_tests = manifest
        .test_files
        .iter()
        .any(|file| app.join(&file.path).is_file());
    if wrote_own_tests {
        PlanOptions {
            min_test_shards: 0,
            ..options
        }
    } else {
        options
    }
}

/// The bench's manifest steps for a staged run, in their order: test
/// spread (only with `engshards` off), tidy, CSS split (`csssplit`, only
/// with `engcss` off; on, the engine's `PlanOptions::split`), then the seed
/// notes. A run that is not staged keeps the manifest as written. With
/// `engtidy` on, the engine's passes (`PlanOptions::tidy`,
/// `one_type_owner`, `today`) replace tidy and the seed notes; only what
/// the options cannot say runs here (see `engine_tidy_rest`). `base` is the
/// built-in profile (not `slim_profile`'s edit of it). The switches are
/// read from `on`; `today` is the run's date.
fn prepare_manifest_with(
    manifest: &mut Manifest,
    staged: bool,
    base: &Profile,
    on: impl Fn(&str) -> bool,
    today: (i32, u32, u32),
) {
    if !staged {
        return;
    }
    spread_tests_with(manifest, &on);
    if on("engtidy") {
        engine_tidy_rest(manifest, base, &on, today);
    } else {
        tidy_manifest_with(manifest, &on);
        // The engine runs it under `PlanOptions::declare_types`, after its
        // tidy passes, as here.
        if on("typedecl") {
            declare_signature_types(manifest, &base.staging);
        }
    }
    if on("csssplit") && !on("engcss") {
        split_css(manifest);
    }
    if !on("engtidy") {
        seed_notes_with(manifest, &on, today);
    }
}

/// The engine passes that the plan options cannot switch one by one: with
/// only one of `onetype` and `oneowner` on, that one pass; with the seed
/// out of the Data layer of the staged profile (`seed` off), the seed note
/// and date on the built-in profile. The seed note and date come after the
/// type passes, as in the bench: both append to the purpose of a seed file
/// that declares a type another file owns. So with the seed moved and both
/// type passes on, `dedupe_type_owners` runs here first too (the plan runs
/// it again under `one_type_owner`, which changes nothing: the pass is
/// idempotent). The plan's other passes touch pages only.
fn engine_tidy_rest(
    manifest: &mut Manifest,
    base: &Profile,
    on: &impl Fn(&str) -> bool,
    today: (i32, u32, u32),
) {
    let seed_moved = on("engstage") && !on("seed");
    let seed_notes = seed_moved && on("seedcompact");
    match (on("onetype"), on("oneowner")) {
        (true, false) => types_files_own_types(manifest, &base.staging),
        (false, true) => first_type_owner_wins(manifest, &base.staging),
        (true, true) if seed_notes => dedupe_type_owners(manifest, base),
        _ => {}
    }
    if seed_notes {
        apply_data_note(manifest, &base.staging);
        if on("seeddates") {
            if on("qrules") {
                stamp_relative_dates(manifest, base);
            } else {
                stamp_today(manifest, base, today);
            }
        }
    }
}

/// The bench's seed notes (`engtidy` off): the compact note
/// (`seedcompact`), then today's date (`seeddates`, only with
/// `seedcompact`), or with `qrules` the engine's relative-date note.
fn seed_notes_with(manifest: &mut Manifest, on: &impl Fn(&str) -> bool, today: (i32, u32, u32)) {
    if on("seedcompact") {
        for file in &mut manifest.files {
            let name = file.path.rsplit('/').next().unwrap_or_default();
            if name.starts_with("seed.") && !file.purpose.ends_with(SEED_NOTE) {
                file.purpose.push_str(SEED_NOTE);
            }
            // Old dates leave a calendar empty and make each record
            // overdue: the demo data must be around the day of the run.
            // `qrules`: the engine's relative-date note instead.
            let relative = slimtoken::profile::NEXTJS.staging.quality.date_note;
            if name.starts_with("seed.") && on("seeddates") && on("qrules") {
                if !file.purpose.contains(relative) {
                    file.purpose.push_str(relative);
                }
                continue;
            }
            if name.starts_with("seed.") && on("seeddates") && !file.purpose.contains("Today is ") {
                let (year, month, day) = today;
                file.purpose.push_str(&format!(
                    " Today is {year:04}-{month:02}-{day:02}: put the dates of the records \
                     in the three weeks before and the three weeks after this day, some in \
                     the past and some in the future."
                ));
            }
        }
    }
}

/// True unless the pipeline step `name` is switched off for an A/B test:
/// the environment variable `SHOWDOWN_OFF` holds a comma-separated list of
/// step names. Each step that can be switched off is a measured improvement;
/// `bench-slim` runs the two arms of such a test.
///
/// The `eng*` switches are temporary: on, a bench step calls its port in
/// the engine; off, the bench copy runs. They go once paired runs show the
/// same results.
///
/// | switch      | on: the engine's                                      | off: the bench's |
/// |-------------|-------------------------------------------------------|------------------|
/// | `engplan`   | `plan_project_with`                                   | planning calls in `plan_slim` |
/// | `engshards` | `PlanOptions::min_test_shards`                        | `spread_tests_with` |
/// | `engstage`  | `PlanOptions::staged` (NEXTJS `staging`)              | `stage_plan` |
/// | `engtidy`   | `PlanOptions::{one_type_owner, tidy, today}`          | `tidy_manifest`, seed notes |
/// | `engcss`    | `PlanOptions::split`, `Staging::aggregate`            | `split_css`, `css_imports` |
/// | `engcheck`  | `check::CommandChecker`/`parse_check`, `project::{scan, safe_path, strip_fence, file_text, apply_edit_blocks}`, `check::apply_patches` (the repair prompt keeps `repair::EDIT_FORMAT`) | `repair::{run_gate_mode, parse_gate, apply_edit_blocks}`, `extract`, `arms::file_text` |
/// | `engrepair` | `repair::check_and_repair` with `profile::NEXTJS_REPAIR` (its `prepare` is the bench's `fix_import_paths` + `fix_mechanical`) and `repair::GateChecker` as the `Checker`; the gate always reads the files through `check::CommandChecker` (also its clean-up in `finish`), while `fix_mechanical` and the final assembly still follow `engcheck` (`engrepair` with `engcheck` off is a mix: keep `engcheck` on) | `repair::check_and_repair` (bench copy) |
/// | `engpipe`   | the whole arm as one `pipeline::build` (plan, wave, files, check and repair) with the options, profile, repair policy and hooks above; the bench parts run in its `BuildHooks` (`PipeHooks`). Off by default ([`DEFAULT_OFF`]); needs `engplan`, `engrepair` and `engcheck` | the arm's own calls (`slim`) |
///
/// The repair switches keep their meaning under `engrepair`:
///
/// | switch      | under `engrepair` |
/// |-------------|-------------------|
/// | `keepdir`   | `RepairPolicy::check_key` (a fresh key), passed to the gate as its directory |
/// | `specdeep`  | `repair::GateChecker` (the bench gate starts the real build beside the fast checks) |
/// | `autofix`   | `fix_mechanical` in the hooks' `prepare` |
/// | `testonly`  | `RepairPolicy::stuck_tests_alone` |
/// | `think`     | `RepairPolicy::escalate_effort` (`medium`; off: `None`) |
/// | `thinktests` | default off: with `think`, a stuck test file's joint repair also gets `medium` reasoning (off: code files only; a test repair with reasoning ran 213 s in race 20261116-051055) |
/// | `softexpect`, `shellcheck`, `notypes` | the gate process (`gate.rs`), unchanged |
/// | `pchk`      | a persistent checker (`pchk.rs`) fed each wave file as it lands; `repair::GateChecker` races it against the command-line fast checks (the first result decides), uses its result only when it comes first and names problems (a pass always comes from the command line; `Checker::confirm` gives the engine the command line's result for an early one that ends the step or is reported) and records the comparison in `slimtoken-repair.json` (`pchk`); with the checker running (only then), the next build waits for the build before it inside its own job, not before the fast checks |
///
/// | `guard`     | `RepairPolicy::guard` (export locks, root first, knock-on files; `slimtoken::guard`) and, in `prepare`, the layout fixes for TS9004/TS9005 (`guard.rs`); lookups in the engine's text index, or in indexio with `SHOWDOWN_INDEX=indexio` (`codeindex.rs`; the executable from `SHOWDOWN_INDEXIO`) |
///
/// A step not yet measured is off unless the comma-separated list in
/// `SHOWDOWN_ON` names it ([`DEFAULT_OFF`]); `SHOWDOWN_OFF` still wins.
pub fn step_on(name: &str) -> bool {
    step_on_with(
        name,
        &std::env::var("SHOWDOWN_ON").unwrap_or_default(),
        &std::env::var("SHOWDOWN_OFF").unwrap_or_default(),
    )
}

/// Steps that are off unless `SHOWDOWN_ON` names them: `examples` (a worked
/// example for each pure calculation in the plan, see `with_examples`),
/// `engpipe` (the arm as one call of the engine's `pipeline::build`, see
/// `slim_pipeline`) and the quality steps, not yet measured live:
/// `qrules` (`PlanOptions::quality_rules`, and `relative_dates` in place of
/// today's date for `seeddates`), `qobjective`
/// (`PlanOptions::objective_pages`) and `qliteral`
/// (`PlanOptions::align_literals`); and the Q2 steps: `qcontract`
/// (`ArchitectOptions::contract` and `PlanOptions::contract`, plus
/// `_meta/slimtoken-contract.json`), `qwiring` (`PlanOptions::wave_wiring`
/// with `wiring_rules`), `qcss` (`PlanOptions::consumers_see_css`, and the
/// same deps in `stage_plan` with `engstage` off) and `prefix`
/// (`PlanOptions::deps_first`); and the Q3 step `qlint` (the gate's
/// cross-file quality lints, `qlint.rs`, as `ERROR` lines for the repair);
/// and `pagelocal` (`ArchitectOptions::page_local` and
/// `PlanOptions::page_local`: a component that one page renders is written
/// in that page's file, shared components are presentational); and the Q4
/// steps `qsemantic` (`ArchitectOptions::semantics` and
/// `PlanOptions::semantics`: the conventions of the types file in every
/// module head, plus `_meta/slimtoken-semantics.json`) and `qlint2` (the
/// gate's convention lints TS9020-TS9025, `qlint.rs`); and the plan
/// coherence steps `typesfirst` (`ArchitectOptions::types_first`: the
/// Types-layer fragment first, its exports in every other fragment's
/// prompt), `planrefs` (`ArchitectOptions::references`: dangling `lib/x fn`
/// and `via name` references go back to their fragments) and `planreview`
/// (`ArchitectOptions::review`: one review call for cross-file problems,
/// asked in the same repair round); and `layered`
/// (`ArchitectOptions::layered`: types, then core, then the rest, the last
/// round with the planned core exports and every such file's job) with
/// `corewhole` (`ArchitectOptions::core_whole`: the core in one fragment);
/// and the transport steps `prewarm` (cheap `GET /models` connections at
/// run start, [`prewarm`]), `hedge` (`slimtoken::hedge`: an identical copy
/// of a request with no first token after a threshold) and `h2` (HTTP/2
/// offered in the TLS handshake), see [`slim_transport`]; and `planpipe`
/// (`ArchitectOptions::plan_pipe`: settled first-round export fragments
/// start while the skeleton streams, kept only when byte-identical); and
/// `inwave` (`slimtoken::inwave`: a landed file whose imports have landed
/// is checked by the persistent checker and gets its repair-loop request
/// while the wave's tail streams; the check and repair after the wave run
/// as before, see [`emit_in_wave`]); and `splitpage`
/// (`ArchitectOptions::split_pages`, with `pagelocal`: each local component
/// of a page of the final plan becomes a part file beside it with a typed
/// contract, written at the same time as the page, see [`emit_split_pages`]);
/// and `skelcompact` (`ArchitectOptions::skel_compact`: the skeleton call
/// answers `path: purpose` lines instead of pretty-printed JSON, about 0.6x
/// the output tokens; another answer falls back to today's request, see
/// [`emit_skel_compact`]); and `shellfix` (`shellfix.rs`: in the repair
/// hooks' `prepare`, a page that wraps its returned JSX in the root layout's
/// frame, TS9021, returns the children instead, with no model call; see
/// [`emit_shell_fix`]), `shellrule` (`PlanOptions::shell_rule`: each page's
/// prompt says that the layout already draws the frame around it) and
/// `inwavelint` (with `inwave` and `qlint2`: the checks during the wave also
/// run the gate's per-file lints and the app lints whose files have all
/// landed, `qlint::landed_lines`; a page waits for the layout and its shell);
/// and `nocut` (`SchedulerConfig::continue_truncated`: an answer cut at its
/// output budget is continued, by prefix completion where the provider has
/// it, and joined instead of written again, see [`emit_no_cut`]) and
/// `filebudget` (`SchedulerConfig::file_min_tokens`: a file request asks for
/// at least 8192 output tokens), see [`no_cut`]; and `typesplan` (with
/// `typesfirst`, `PlanOptions::types_from_plan`: the `lib` modules and the
/// seed data start with the wave on the planned type declarations instead of
/// waiting 2.5-3.3 s for the types file, which repeats them word for word);
/// and `fencestop` (`TapClient::with_fence_stop`: a file answer that goes on
/// in prose after its code block has closed is stopped there; 4 of 1,121
/// file answers in 27 runs did, for 5-26 s each); `jointedit`
/// (`RepairPolicy::joint_edits`: a first repair whose problems name another
/// file answers with edit blocks per file, not whole files); and `testedit`
/// (`RepairPolicy::test_edits`: a test file is repaired by edit blocks, alone
/// and together with its modules, however long the checker's report is);
/// and `specwave` (`EarlyWave`: the Types, Core and Data files and the wiring
/// start on the manifest that the contract repair's model round starts from,
/// about 3 s before the final plan; a file that the repair leaves as planned
/// keeps its early text, the others are written again at once); and
/// `speedrace` (`TapClient::with_speed_race`, `slimtoken::speedrace`: three
/// copies of each plan and file request, of which the one that streams
/// fastest in the first 1.5 s is kept; DeepSeek streams run at about 240 or
/// about 380 tokens/s and the class shows at once); and `pagefork`
/// (`slimtoken::pagefork`, `crate::tap::PageForkTap`: a page's writer writes
/// the imports, the props type of each local component and the default
/// export; each local component is written by a request of its own, started
/// when the default export starts, and joined into the page's file).
pub const DEFAULT_OFF: &[&str] = &[
    "examples",
    "engpipe",
    "qrules",
    "qobjective",
    "qliteral",
    "qcontract",
    "qwiring",
    "qcss",
    "prefix",
    "qlint",
    "pagelocal",
    "thinktests",
    "qsemantic",
    "qlint2",
    "typesfirst",
    "planrefs",
    "planreview",
    "layered",
    "corewhole",
    "prewarm",
    "hedge",
    "h2",
    "planpipe",
    "typespaths",
    "contractside",
    "inwave",
    "splitpage",
    "skelcompact",
    "testsee",
    "shellfix",
    "shellrule",
    "inwavelint",
    "nocut",
    "filebudget",
    "typesplan",
    "fencestop",
    "jointedit",
    "testedit",
    "specwave",
    "speedrace",
    "pagefork",
];

/// The switches the `showdown` command turns on when `SHOWDOWN_ON` is not
/// set (`main` puts this list in the environment, so the gate and the other
/// child processes see it too; `SHOWDOWN_ON=` set empty keeps every
/// [`DEFAULT_OFF`] step off). The library and its tests keep the fixed
/// baseline above. Measured 2026-11-16 with blind Claude audits on DeepSeek
/// runs: with all of these, SlimToken TaskFlow apps scored 84 against 63 for
/// the same stack without `qsemantic`/`qlint2` (8 of 8 judgments) and
/// Ledgerly 74 against 57. Left out: `prefix` (TaskFlow 4v4: wave 18.5 s on,
/// 18.3 s off, cache hits +2 points) and `thinktests` (reasoning on a test
/// repair stalled 173-213 s with no answer). `typesfirst` (held-out set B,
/// 2 runs per app, blind audits): SlimToken 78.4 and 72.9 against 69.2
/// without it, for +3-4 s of planning. Also left out: `planrefs` and
/// `planreview` (with `typesfirst`, 73.6 in one run, +3 s more planning).
/// `shellfix`, `nocut` and `filebudget` (2026-11-17, set B): they act only
/// on a page that fails the duplicate-shell lint (deterministic unwrap), a
/// file cut at its output budget (continued instead of restarted) and the
/// budget itself; with them and `inwave`/`inwavelint` SlimToken took 38.7 s
/// against 56.0 s for `typesfirst` alone (one 93 s run there was a restart
/// after a cut) and passed every test (0 of 4 vs 2 of 4 failing). Left out:
/// `skelcompact` (blind head-to-head 72.6 against 80.9, 0 of 8 pairs: plans
/// lost data-model detail, for 0.5 s) and `inwave`/`inwavelint` (that stack
/// without `skelcompact` scored 74.7 against 81.0, 3 of 8 pairs, while a
/// fresh `typesfirst` control scored 80.8 against 79.4; not yet isolated).
/// `typesplan`, `jointedit`, `testedit`, `specwave` and `hedge` (2026-11-18,
/// set B, alternating pairs against the list without them): faster in 8 of
/// 9 pairs (-9%) and, after `specwave`'s keep rule was narrowed, in 7 of 8
/// (48.5 s against 55.2 s, -12%; first file at 16.0 s against 20.5 s), 25
/// of 25 runs passing against 24 of 25; the bench's blind judge on TaskFlow
/// and Ledgerly, two runs each: 94.6 against 94.9. `hedge` is insurance
/// against a stalled request (5 in 25,924 streams, 10-25 s each). Left out:
/// `fencestop` (it never fired), `speedrace` and `pagefork` (measured,
/// rejected: see the CHANGELOG). `contractside` (2026-11-18): a quality
/// step, not a speed step. In 13 of 42 failing test cases the module had
/// broken its contract and the test was rewritten to match; with it the
/// module is repaired (two saved cases replayed, and one live run of 9),
/// 13 live verdicts with no module named wrongly, 42.3 s against 41.8 s
/// over 9 alternating pairs.
pub const RECOMMENDED_ON: &[&str] = &[
    "qrules",
    "qobjective",
    "qliteral",
    "qcontract",
    "qwiring",
    "qcss",
    "qlint",
    "pagelocal",
    "qsemantic",
    "qlint2",
    "typesfirst",
    "shellfix",
    "nocut",
    "filebudget",
    "typesplan",
    "jointedit",
    "testedit",
    "specwave",
    "hedge",
    "contractside",
];

/// `step_on` with the two lists given.
fn step_on_with(name: &str, on: &str, off: &str) -> bool {
    let listed = |list: &str| list.split(',').any(|item| item.trim() == name);
    if listed(off) {
        return false;
    }
    !DEFAULT_OFF.contains(&name) || listed(on)
}

/// Added to the NEXTJS edge rule of the planning call with `examples`.
const EXAMPLE_EDGE_RULE: &str = "
- A pure `lib/` function that computes a number, a date or formatted text
  (a total, an average, a percentage, a count of days, a series, a label,
  a snippet) ends its comment with one worked example on small literal
  inputs, `e.g. f(...) -> result`, where the result is the exact value the
  code returns (e.g. `export function percentDone(done: number, total:
  number): number  // total===0 -> 0; e.g. percentDone(1, 3) -> 33`). The
  inputs are literals: no today's date, no generated ids.";

/// Added to the rules of the module prompts with `examples`.
const EXAMPLE_MODULE_RULE: &str = "- An export whose contract ends with `e.g. f(...) -> \
result` returns exactly that result for those inputs: work the example through your code before \
you write it.\n";

/// Added to the rules of the test prompts with `examples`.
const EXAMPLE_TEST_RULE: &str = "- Where a contract ends with `e.g. f(...) -> result`, test \
that call with exactly that expected value, and work out the other expected values the same \
way.\n";

/// `base` with the `examples` rules: in the edge rule of the planning call
/// (each pure calculation gets one `e.g. f(...) -> result`), and in the
/// module and test prompt heads (the code satisfies the example, the tests
/// assert it). Tests and code then come from the same examples. Only for
/// NEXTJS (the rules are in TypeScript syntax); any other profile is
/// returned as it is.
fn with_examples(base: &'static Profile) -> &'static Profile {
    if base.name != slimtoken::profile::NEXTJS.name {
        return base;
    }
    let module_head = format!(
        "{}\n{EXAMPLE_MODULE_RULE}\n",
        base.module_head.trim_end_matches('\n')
    );
    let test_head = base.test_head.replacen(
        "- Keep tests compact.\n",
        &format!("- Keep tests compact.\n{EXAMPLE_TEST_RULE}"),
        1,
    );
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    Box::leak(Box::new(Profile {
        edge_rule: leak(format!("{}{EXAMPLE_EDGE_RULE}", base.edge_rule)),
        module_head: leak(module_head),
        test_head: leak(test_head),
        ..*base
    }))
}

/// The `slim` arm's transport switches on `client`: `h2` (offer HTTP/2 in
/// the TLS handshake; off, HTTP/1.1 only) and `hedge` (an identical copy of
/// a request with no first token after `SHOWDOWN_HEDGE_MS`, default 2500;
/// at most `SHOWDOWN_HEDGE_MAX`, default 16, per run; the copy whose first
/// token comes first wins and the other is dropped). Both off by default
/// ([`DEFAULT_OFF`]).
fn slim_transport(client: TapClient, on: impl Fn(&str) -> bool) -> Result<TapClient> {
    let client = if on("h2") {
        client.with_http2(true)?
    } else {
        client
    };
    Ok(client
        .with_hedge(on("hedge").then(hedge_config))
        .with_nocut(on("nocut"))
        .with_fence_stop(on("fencestop"))
        .with_speed_race(on("speedrace")))
}

/// The output tokens from which a cut first answer counts as a runaway
/// (`no_cut`): just under the 8,192-token budget of a file.
const RUNAWAY_TOKENS: u64 = 8_000;

/// `nocut` and `filebudget` (both off unless `SHOWDOWN_ON` names them) in
/// the scheduler: a wave answer cut at its output budget is continued up to
/// `slimtoken::continuation::DEFAULT_CAP` times
/// (`SchedulerConfig::continue_truncated`, the client's side in
/// [`TapClient::with_nocut`]), and a file task's request asks for at least
/// `SHOWDOWN_FILE_TOKENS` (default 8192) output tokens
/// (`SchedulerConfig::file_min_tokens`). With both, a first answer cut at
/// `RUNAWAY_TOKENS` or more is not continued
/// (`SchedulerConfig::continue_runaway_tokens`): with that floor a cut is a
/// runaway, not a long file.
fn no_cut(scheduler: &mut SchedulerConfig, on: impl Fn(&str) -> bool) {
    if on("nocut") {
        scheduler.continue_truncated = Some(slimtoken::continuation::DEFAULT_CAP);
        if on("filebudget") {
            scheduler.continue_runaway_tokens = Some(RUNAWAY_TOKENS);
        }
    }
    if on("filebudget") {
        let floor = std::env::var("SHOWDOWN_FILE_TOKENS")
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(FILE_TOKENS);
        scheduler.file_min_tokens = Some(floor);
    }
}

/// `filebudget`'s least output budget of a file request.
const FILE_TOKENS: u32 = 8_192;

/// `speedrace`: what the client raced up to the end of the wave, as one
/// `speedrace` tool event (only with the switch on): `files` the requests
/// raced, `hits` the races that the window decided.
fn emit_speed_race(emitter: &Emitter, arm: Arm, client: &TapClient) {
    let Some(stats) = client.speed_stats() else {
        return;
    };
    emitter.emit(Ev::Tool {
        arm,
        tool: "speedrace".to_string(),
        op: "summary".to_string(),
        ms: 0,
        critical_ms: 0,
        files: stats.races as _,
        hits: stats.chosen as _,
        note: format!(
            "races={} copies={} chosen={} dropped_chars={}",
            stats.races, stats.copies, stats.chosen, stats.dropped_chars
        ),
    });
}

/// `nocut`: what the client continued in the wave, as one `nocut` tool
/// event (only with the switch on): `ms` the estimated time saved
/// ([`crate::tap::NoCutStats::saved_ms`]), `critical_ms` the continuations'
/// time, `files` the cut file streams, `hits` the files joined whole.
fn emit_no_cut(emitter: &Emitter, arm: Arm, client: &TapClient) {
    let Some((stats, prefix)) = client.nocut_stats() else {
        return;
    };
    emitter.emit(Ev::Tool {
        arm,
        tool: "nocut".to_string(),
        op: "summary".to_string(),
        ms: stats.saved_ms,
        critical_ms: stats.continue_ms,
        files: stats.truncated,
        hits: stats.joined,
        note: format!(
            "truncated={} continuations={} prefix={} turn={} failed={} prefix_refused={} prefix_errors={} prefix_note={:?} joined={} cut_ms={} continue_ms={} saved_ms={} prefix_on={} files={}",
            stats.truncated,
            stats.continuations,
            stats.prefix,
            stats.turn,
            stats.failed,
            stats.prefix_refused,
            stats.prefix_errors,
            stats.prefix_note.as_deref().unwrap_or(""),
            stats.joined,
            stats.cut_ms,
            stats.continue_ms,
            stats.saved_ms,
            u8::from(prefix),
            stats.files.join(" "),
        ),
    });
}

/// `hedge`'s threshold and budget, from `SHOWDOWN_HEDGE_MS` and
/// `SHOWDOWN_HEDGE_MAX` when they parse.
fn hedge_config() -> slimtoken::hedge::HedgeConfig {
    let number = |name: &str| -> Option<usize> {
        std::env::var(name)
            .ok()
            .and_then(|text| text.trim().parse().ok())
    };
    let mut config = slimtoken::hedge::HedgeConfig::default();
    if let Some(ms) = number("SHOWDOWN_HEDGE_MS") {
        config.after = std::time::Duration::from_millis(ms as u64);
    }
    if let Some(max) = number("SHOWDOWN_HEDGE_MAX") {
        config.max_hedges = max;
    }
    config
}

/// True with `inwave` on (off unless `SHOWDOWN_ON` names it) and what it
/// needs: the persistent checker (`pchk`) and the engine's repair loop
/// (`engrepair`).
fn in_wave_on(on: impl Fn(&str) -> bool) -> bool {
    on("inwave") && on("pchk") && on("engrepair")
}

/// `inwave`: each repaired file goes to the persistent checker as it
/// answers, so its compile and a Vitest run of its tests start before the
/// check after the wave.
fn in_wave_feed(sidecar: Arc<crate::pchk::Sidecar>) -> slimtoken::inwave::RepairedHook {
    Arc::new(move |title, output| sidecar.land(title, output))
}

/// The in-wave repair (`inwave` A/B) as one `tool` event, on every run whose
/// check ran (switch off: no repairs, the after-wave time alone): `ms` is
/// `after_wave_ms` (the check and repair after the wave), `critical_ms`
/// `in_wave_ms` (first in-wave request to its last answer), `files` the
/// in-wave repairs, `hits` the ones put in place, and the note every count
/// and time (`tail_wait_ms`: after the wave, the wait for in-wave repairs).
fn emit_in_wave(
    emitter: &Emitter,
    arm: Arm,
    in_wave: Option<&slimtoken::inwave::InWaveReport>,
    after_wave_ms: u64,
) {
    let none = slimtoken::inwave::InWaveReport::default();
    let report = in_wave.unwrap_or(&none);
    emitter.emit(Ev::Tool {
        arm,
        tool: "inwave".to_string(),
        op: "summary".to_string(),
        ms: after_wave_ms,
        critical_ms: report.in_wave_ms,
        files: report.in_wave_repairs,
        hits: report.applied,
        note: format!(
            "on={} in_wave_repairs={} applied={} skipped={} checks={} check_ms={} first_repair_ms={} in_wave_ms={} wave_ms={} tail_wait_ms={} after_wave_ms={after_wave_ms} output_tokens={} files={}",
            u8::from(in_wave.is_some()),
            report.in_wave_repairs,
            report.applied,
            report.skipped,
            report.checks,
            report.check_ms,
            report
                .first_repair_ms
                .map_or_else(|| "-".to_string(), |ms| ms.to_string()),
            report.in_wave_ms,
            report.wave_ms,
            report.tail_wait_ms,
            report.output_tokens,
            report.files.join(" "),
        ),
    });
}

/// `shellfix` as one `tool` event on each run whose check ran with the
/// switch on: `files` is `shellfix_applied` (pages whose layout frame the
/// repair hooks' `prepare` unwrapped, during the wave's checks or after it,
/// each once), `hits` the pages that the lint named but whose change was
/// not clear (left for the repair); the note lists both.
fn emit_shell_fix(
    emitter: &Emitter,
    arm: Arm,
    (fixed, left): &(
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
    ),
) {
    let join = |set: &std::collections::BTreeSet<String>| {
        set.iter().cloned().collect::<Vec<_>>().join(" ")
    };
    emitter.emit(Ev::Tool {
        arm,
        tool: "shellfix".to_string(),
        op: "summary".to_string(),
        ms: 0,
        critical_ms: 0,
        files: fixed.len(),
        hits: left.len(),
        note: format!(
            "shellfix_applied={} left={} fixed_files={} left_files={}",
            fixed.len(),
            left.len(),
            join(fixed),
            join(left)
        ),
    });
}

/// True with `splitpage` on (off unless `SHOWDOWN_ON` names it) and what it
/// needs: the page-local fold (`pagelocal`), whose notes name the parts.
fn split_page_on(on: impl Fn(&str) -> bool) -> bool {
    on("splitpage") && on("pagelocal")
}

/// The page split (`splitpage` A/B) as one `tool` event in every slim run,
/// after the wave: `ms` is the wave's time, `critical_ms` the slowest page or
/// part (the tail the split shortens), `files` the parts, `hits` the split
/// pages, and the note every count and time of the split and the slowest
/// page and part (switch off: `on=0`, no parts, the same page times).
fn emit_split_pages(
    emitter: &Emitter,
    arm: Arm,
    sink: Option<&slimtoken::splitpage::SplitSink>,
    report: &slimtoken::RunReport,
) {
    let split = sink
        .and_then(|sink| sink.lock().ok().and_then(|slot| slot.clone()))
        .unwrap_or_default();
    let quality = &slimtoken::profile::NEXTJS.staging.quality;
    let part_dir = format!("/{}/", quality.page_split.dir);
    let slowest = |keep: &dyn Fn(&str) -> bool| {
        report
            .tasks
            .values()
            .filter(|task| keep(&task.title))
            .max_by_key(|task| task.duration_ms)
            .map(|task| (task.title.clone(), task.duration_ms, task.output_tokens))
    };
    let page = slowest(&|title| slimtoken::profile::glob_match(quality.page_glob, title));
    let part = slowest(&|title| title.contains(&part_dir));
    let show = |task: &Option<(String, u64, u64)>| {
        task.as_ref().map_or_else(
            || "-".to_string(),
            |(title, ms, tokens)| format!("{title}:{ms}ms:{tokens}tok"),
        )
    };
    let critical = page
        .iter()
        .chain(part.iter())
        .map(|(_, ms, _)| *ms)
        .max()
        .unwrap_or(0);
    emitter.emit(Ev::Tool {
        arm,
        tool: "splitpage".to_string(),
        op: "summary".to_string(),
        ms: report.elapsed_ms,
        critical_ms: critical,
        files: split.parts,
        hits: split.pages.len(),
        note: format!(
            "on={} split_pages={} parts={} candidates={} planned_signatures={} props_calls={} props_failed={} props_rejected={} props_ms={} props_max_ms={} parts_capped={} props_input_tokens={} props_output_tokens={} split_ms={} slowest_page={} slowest_part={} wave_ms={} pages={}",
            u8::from(sink.is_some()),
            split.pages.len(),
            split.parts,
            split.candidates,
            split.planned_signatures,
            split.props_calls,
            split.props_failed,
            split.props_rejected,
            split.props_ms,
            split.props_max_ms,
            split.parts_capped,
            split.props_input_tokens,
            split.props_output_tokens,
            split.split_ms,
            show(&page),
            show(&part),
            report.elapsed_ms,
            split
                .pages
                .iter()
                .map(|page| page.page.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
    });
}

/// The planning timeline (`planpipe` A/B) as one `tool` event: `ms` is
/// planning's end (the clock's `planning_ms`), `critical_ms` the first wave
/// file's open, the note every step on the client's clock (ms from the
/// arm's start, `-` if unseen) and `planning_ms` on the planning clock.
fn emit_plan_timeline(emitter: &Emitter, arm: Arm, client: &TapClient, planning_ms: u64) {
    let timeline = client.timeline();
    let show = |ms: Option<u64>| ms.map_or_else(|| "-".to_string(), |ms| ms.to_string());
    emitter.emit(Ev::Tool {
        arm,
        tool: "planpipe".to_string(),
        op: "timeline".to_string(),
        ms: planning_ms,
        critical_ms: timeline.wave_first_open_ms.unwrap_or(0),
        files: 0,
        hits: usize::from(step_on("planpipe")),
        note: format!(
            "skeleton_ms={} first_fragment_start_ms={} planning_done_ms={} wave_first_open_ms={} planning_ms={planning_ms}",
            show(timeline.skeleton_ms),
            show(timeline.first_fragment_start_ms),
            show(timeline.planning_done_ms),
            show(timeline.wave_first_open_ms),
        ),
    });
}

/// The compact skeleton (`skelcompact` A/B) as one `tool` event, only with
/// the switch on: `ms` is the skeleton step on the planner's clock,
/// `critical_ms` the compact call alone, `files` the skeleton's files, `hits`
/// 1 when the compact answer was used (lines or JSON), and the note the form,
/// the answer's size and output tokens, the fallback's reason and the
/// client's `skeleton_ms` (as in the `planpipe` timeline).
fn emit_skel_compact(
    emitter: &Emitter,
    arm: Arm,
    client: &TapClient,
    sink: Option<&slimtoken::skelcompact::SkelSink>,
) {
    let Some(sink) = sink else {
        return;
    };
    let report = sink
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_default();
    use slimtoken::skelcompact::SkelFormat;
    let used = matches!(report.format, SkelFormat::Lines | SkelFormat::Json);
    emitter.emit(Ev::Tool {
        arm,
        tool: "skelcompact".to_string(),
        op: report.format.as_str().to_string(),
        ms: report.skeleton_ms,
        critical_ms: report.call_ms,
        files: report.files,
        hits: usize::from(used),
        note: format!(
            "format={} call_ms={} skeleton_ms={} chars={} output_tokens={} files={} timeline_skeleton_ms={} reason={}",
            report.format.as_str(),
            report.call_ms,
            report.skeleton_ms,
            report.chars,
            report.output_tokens,
            report.files,
            client
                .timeline()
                .skeleton_ms
                .map_or_else(|| "-".to_string(), |ms| ms.to_string()),
            report.reason.chars().take(160).collect::<String>(),
        ),
    });
}

/// `prewarm`: open `SHOWDOWN_PREWARM` (default 40, at most 128) provider
/// connections with cheap `GET {base}/models` requests beside the first
/// planning call (not awaited), so the wave's streams skip connect and TLS.
/// The time and the count go to the recording as a `prewarm` tool event.
fn prewarm(client: Arc<TapClient>, emitter: Emitter, arm: Arm) {
    let connections = std::env::var("SHOWDOWN_PREWARM")
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(slimtoken::client::DEFAULT_PREWARM_CONNECTIONS);
    tokio::spawn(async move {
        if let Ok(report) = client.prewarm(connections).await {
            emitter.emit(Ev::Tool {
                arm,
                tool: "prewarm".to_string(),
                op: "done".to_string(),
                ms: report.elapsed.as_millis() as u64,
                critical_ms: 0,
                files: report.answered,
                hits: report.requested,
                note: format!("{} of {} connections", report.answered, report.requested),
            });
        }
    });
}

/// Streams that the scheduler lets start from the first second.
const START_WIDTH: usize = 64;

/// Start the wave wide. The engine sets its first limit from the number of
/// files that are ready at the start. A staged plan has only the types files
/// ready then, so the limit starts near 5 and the 20 files of the next stage
/// wait in a queue while the limit grows. The limit still goes down when the
/// provider shows overload.
fn wide_start(scheduler: &mut SchedulerConfig) {
    if !step_on("wide") {
        return;
    }
    // A request that waits for its first token three times as long as
    // the median of the wave (and at least 5 s) starts again: one such
    // request held a whole stage for 24 s on DeepSeek. The provider preset
    // decides (DeepSeek sets 3, applied above); Muse sets none, because its
    // first token often takes 5-12 s and the watchdog only restarted
    // requests that were about to answer.
    if !step_on("watchdog") {
        scheduler.first_token_watchdog = None;
    }
    scheduler.controller.cold_hint_cap = 0;
    scheduler.controller.start_percent = 100;
    scheduler.controller.cold_burst_cap = START_WIDTH;
}

/// Most test files that the plan writes at the same time (the engine's
/// limit for test shards).
const TEST_FILES: usize = 6;

/// Ask for the tests in some files, not in one. One test file for all
/// modules is the longest file of the app (about 4,400 tokens, 14 s), so it
/// alone sets the time of the last stage and of each repair round that
/// changes a test. The engine writes one test file for each entry in the
/// manifest, each for its own group of modules, at the same time. Used only
/// with `engshards` off; on, the engine's `min_test_shards` does this. The
/// switches are read from `on` (`prepare_manifest_with` passes `step_on`).
fn spread_tests_with(manifest: &mut slimtoken::architect::Manifest, on: impl Fn(&str) -> bool) {
    if !on("shards") || on("engshards") {
        return;
    }
    let Some(first) = manifest.test_files.first().cloned() else {
        return;
    };
    while manifest.test_files.len() < TEST_FILES {
        let mut extra = first.clone();
        extra.path = format!("tests/part{}.test.ts", manifest.test_files.len() + 1);
        manifest.test_files.push(extra);
    }
}

/// True for a file of shared types.
fn is_types_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "types.ts" || name.ends_with(".d.ts") || path.contains("/types/")
}

/// The name of the type that a plan line declares (`export interface Task`,
/// `export type Status = ...`), if it declares one.
fn planned_type(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("export ")?;
    let rest = ["interface ", "type ", "enum "]
        .iter()
        .find_map(|word| rest.strip_prefix(word))?;
    let end = rest
        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// Take two contradictions out of the plan before a worker reads it. The
/// plan is written by many small requests at the same time, so it can tell
/// two files to declare the same type (with different fields), and it plans
/// a page with a route parameter as a Server Component that then needs a
/// client file which the plan does not have. Each of these was in almost
/// every first check, and each costs a repair round. The bench copy, used
/// with `engtidy` off; on, the engine's `dedupe_type_owners` and NEXTJS
/// `tidy_manifest` hook do this. The switches are read from `on`.
fn tidy_manifest_with(manifest: &mut Manifest, on: &impl Fn(&str) -> bool) {
    if on("onetype") {
        let owner = manifest
            .files
            .iter()
            .find(|file| is_types_path(&file.path))
            .map(|file| file.path.clone());
        let owned: Vec<String> = manifest
            .files
            .iter()
            .filter(|file| is_types_path(&file.path))
            .flat_map(|file| file.exports.iter())
            .filter_map(|line| planned_type(line))
            .map(str::to_string)
            .collect();
        if let Some(owner) = owner {
            for file in &mut manifest.files {
                if is_types_path(&file.path) {
                    continue;
                }
                let mut moved = Vec::new();
                file.exports.retain(|line| match planned_type(line) {
                    Some(name) if owned.iter().any(|own| own == name) => {
                        moved.push(name.to_string());
                        false
                    }
                    _ => true,
                });
                if !moved.is_empty() {
                    file.purpose.push_str(&format!(
                        " Imports {} from `{owner}`: this file does not declare or export a type with that name, and it uses the fields and literal values of `{owner}`.",
                        moved.join(", ")
                    ));
                }
            }
        }
    }
    // An app with no server code keeps its data in the browser, so a page
    // that the plan made an `async` Server Component (to read the query
    // string) cannot read that data: the worker then adds hooks to it and
    // the build fails.
    let has_server = manifest.files.iter().any(|file| {
        file.path.contains("/api/")
            || file.path.ends_with("route.ts")
            || file.path.contains("actions")
    });
    if on("clientpages") && !has_server {
        for file in &mut manifest.files {
            let is_page = file.path.ends_with("page.tsx") && !file.path.contains("/[");
            // Any plan line that reads `searchParams`, unless it is a Client
            // Component (as the engine's `next_client_pages`).
            let reads_params =
                |line: &str| line.contains("searchParams") && !line.contains("Client Component");
            if !is_page || !file.exports.iter().any(|line| reads_params(line)) {
                continue;
            }
            let has_default = file
                .exports
                .iter()
                .any(|line| line.contains("export default"));
            for line in &mut file.exports {
                let page_line = if has_default {
                    line.contains("export default")
                } else {
                    reads_params(line)
                };
                if !page_line || line.contains("Client Component") {
                    continue;
                }
                let Some(name) = line
                    .split("function ")
                    .nth(1)
                    .and_then(|rest| rest.find('(').map(|end| rest[..end].trim().to_string()))
                else {
                    continue;
                };
                // Without the clauses that await the query string (as the
                // engine's `next_client_pages`).
                let note = slimtoken::profile::client_page_note(
                    line.split_once("//").map_or("", |(_, note)| note.trim()),
                );
                *line = format!(
                    "export default function {name}()  // Client Component: the first line is \
                     \"use client\"; an inner component reads the query string with \
                     useSearchParams() from next/navigation and the page renders it inside \
                     <Suspense>; {note}"
                );
            }
        }
    }
    // A type that two files outside the types file declare (two `AppState`
    // in `lib/storage.ts` and `lib/store.ts`): the first file keeps it.
    if on("oneowner") {
        let mut owners: Vec<(String, String)> = Vec::new();
        for file in &mut manifest.files {
            if is_types_path(&file.path) || !file.path.ends_with(".ts") {
                continue;
            }
            let path = file.path.clone();
            let mut moved: Vec<(String, String)> = Vec::new();
            file.exports.retain(|line| {
                let Some(name) = planned_type(line) else {
                    return true;
                };
                match owners.iter().find(|(own, _)| own == name) {
                    Some((_, owner)) => {
                        moved.push((name.to_string(), owner.clone()));
                        false
                    }
                    None => {
                        owners.push((name.to_string(), path.clone()));
                        true
                    }
                }
            });
            for (name, owner) in moved {
                file.purpose.push_str(&format!(
                    " Imports {name} from `{owner}`: this file does not declare a type with \
                     that name."
                ));
            }
        }
    }
    if on("dynroute") {
        for file in &mut manifest.files {
            let is_page = file.path.ends_with("/page.tsx") && file.path.contains("/[");
            if !is_page
                || file
                    .exports
                    .iter()
                    .any(|line| line.contains("Client Component"))
            {
                continue;
            }
            let page_line = file
                .exports
                .iter()
                .find(|line| line.contains("export default") && line.contains("function "))
                .or_else(|| file.exports.iter().find(|line| line.contains("function ")));
            let name = page_line
                .and_then(|line| {
                    let rest = line.split("function ").nth(1)?;
                    let end = rest.find('(')?;
                    Some(rest[..end].trim().to_string())
                })
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Page".to_string());
            let note = page_line
                .and_then(|line| line.split_once("//"))
                .map(|(_, note)| note.trim())
                .filter(|note| !note.is_empty())
                .map(|note| format!("; {note}"))
                .unwrap_or_default();
            file.exports = vec![format!(
                "export default function {name}()  // Client Component: the first line is \
                 \"use client\"; reads the route parameter with useParams() from \
                 next/navigation; returns a not-found message when no record has that id, \
                 before any other use of the record; a handler that uses the record is an arrow \
                 function in a const (`const save = () => ...`), never a `function` declaration \
                 (inside a hoisted function TypeScript forgets that the record was found); all the \
                 code of the page is in this file, \
                 apart from the planned components it renders{note}"
            )];
        }
    }
}

/// The variables that all parts of the style sheet use. The first part
/// defines them, so the parts agree without seeing each other.
const CSS_VARIABLES: &str = "--bg, --surface, --surface-2, --text, --muted, --border, --accent, \
--accent-contrast, --danger, --success, --warning, --radius, --shadow";

/// Classes for one part of the style sheet.
const CSS_PART_CLASSES: usize = 7;

/// Have some streams write the global style sheet together. It is the
/// longest file that starts with the wave (about 3,000 tokens, 10 s), and a
/// style sheet joins without conflicts: each stream writes the rules of its
/// own classes into its own part, and the planned file only imports the
/// parts. All parts get the same prompt prefix, so the cache still hits.
/// The parts use one fixed list of variables that the first part defines.
/// The bench copy, used with `engcss` off; on, the engine's
/// `PlanOptions::split` with the NEXTJS `staging.split` does this, and its
/// `aggregate` hook renders the joined file (`css_imports`).
fn split_css(manifest: &mut slimtoken::architect::Manifest) {
    let Some(index) = manifest.files.iter().position(|file| {
        file.path.ends_with("globals.css") && file.exports.len() >= 2 * CSS_PART_CLASSES
    }) else {
        return;
    };
    let whole = manifest.files[index].clone();
    let dir = whole.path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let count = whole.exports.len().div_ceil(CSS_PART_CLASSES).clamp(2, 5);
    let size = whole.exports.len().div_ceil(count);
    let mut parts = Vec::new();
    for (number, classes) in whole.exports.chunks(size).enumerate() {
        let mut part = whole.clone();
        part.path = if dir.is_empty() {
            format!("styles/part{}.css", number + 1)
        } else {
            format!("{dir}/styles/part{}.css", number + 1)
        };
        part.exports = classes.to_vec();
        part.purpose = if number == 0 {
            format!(
                "Part 1 of {count} of the global style sheet. Starts with the reset, the \
                 `body` and base element styles, and the variables {CSS_VARIABLES} on `:root` \
                 with their dark values on `.theme-dark`. Then only the rules of the classes \
                 listed for this part (with their hover, focus and small-screen rules). All \
                 colours come from the variables."
            )
        } else {
            format!(
                "Part {} of {count} of the global style sheet: only the rules of the classes \
                 listed for this part (with their hover, focus and small-screen rules). No \
                 reset, no `:root`, no `body` rules. All colours, radii and shadows come from \
                 these variables, which part 1 defines for the light and the dark theme: \
                 {CSS_VARIABLES}.",
                number + 1
            )
        };
        parts.push(part);
    }
    manifest.files[index].exports.clear();
    manifest.files[index].purpose = "Imports the parts of the global style sheet.".to_string();
    manifest.files.splice(index + 1..index + 1, parts);
}

/// Render each style sheet that only imports its parts as the bench does
/// (`engcss` off): its fixed text, in a code block as a worker gives it (the
/// gate reads blocks). With `engcss` on, the engine has rendered it already
/// (`PlanOptions::split`, the NEXTJS `aggregate` hook). The switches are
/// read from `on`.
fn render_css_with(plan: &mut Plan, manifest: &Manifest, on: impl Fn(&str) -> bool) {
    if on("engcss") {
        return;
    }
    for (path, text) in css_imports(manifest) {
        if let Some(task) = plan.tasks.iter_mut().find(|task| task.title == path) {
            task.static_output = Some(format!("```css\n{}\n```", text.trim_end()));
        }
    }
}

/// The fixed text of a style sheet that only imports its parts, for each
/// such file in the plan: `(path, text)`.
fn css_imports(manifest: &slimtoken::architect::Manifest) -> Vec<(String, String)> {
    manifest
        .files
        .iter()
        .filter(|file| file.path.ends_with("globals.css") && file.exports.is_empty())
        .filter_map(|file| {
            let dir = file.path.rsplit_once('/').map_or("", |(dir, _)| dir);
            let prefix = if dir.is_empty() {
                "styles/part".to_string()
            } else {
                format!("{dir}/styles/part")
            };
            let text: String = manifest
                .files
                .iter()
                .filter(|part| part.path.starts_with(&prefix) && part.path.ends_with(".css"))
                .map(|part| {
                    format!(
                        "@import \"./{}\";\n",
                        part.path[dir.len()..].trim_start_matches('/')
                    )
                })
                .collect();
            (!text.is_empty()).then(|| (file.path.clone(), text))
        })
        .collect()
}

/// Added to the plan of the seed data: the same data in fewer tokens.
const SEED_NOTE: &str = " Write each record as one compact object literal on one line, with \
short realistic text values. Generate repeating records (one per month or week the pages show, \
per-period records such as budgets included) with a loop over the periods instead of listing \
each one, so the file stays under 120 lines while every period has data. Every collection and \
sub-collection that the pages show (comments, subtasks, archived records) has at least two \
records.";

/// Added to the prompt of each `lib` file that is written after the types.
const TYPES_WIN_RULE: &str = "\n\nThe attached types file is the authority. If a line of the \
plan for this file names a field, a literal value or a parameter shape that the attached types \
do not have, use the names and the shapes of the attached types, and keep the name and the \
purpose of each planned function. A parameter that takes domain records has the shared type \
(`Item[]` for a shared type `Item`), not an inline object shape (`{ name: string }[]`). A type that holds the whole \
app state has one collection for each entity type that the attached types declare.";

/// Added to the prompt of each `.tsx` file.
const NEXT_RULE: &str = "\n\nNext.js rules: a file that uses React hooks, event handlers or \
browser APIs starts with the line \"use client\". A component that calls useSearchParams() is \
an inner component of the same file, and the file renders it inside <Suspense>. Import each \
component in the form that the plan declares for it: `import Name from` for `export default`, \
`import { Name } from` for a named export. Move between the app's pages only with `<Link>` from \
next/link or `router.push`, never with `<a href>` to an app route or `window.location` (external \
links and downloads keep `<a>`).";

/// Added to the prompt of each test file.
const TEST_RULE: &str = "\n\nTest rules: read the attached code of a function before you \
assert on it, and work out each expected value from that code with a small input (one to three \
items). Do not assert a value that depends on today's date, on a generated id or on the order \
of equal items. A few certain assertions are better than many uncertain assertions. For a function whose code is not attached (the seed data), assert only what holds for any data: collections that are not empty, unique ids, references that resolve; never an exact count or value. Give \
test data the shared types (`const items: Item[] = [...]` for a shared type `Item`) and make each record with a small \
factory function that sets every required field, so that literal values keep their types.";

/// Added to the prompt of every file that a worker writes.
const IMPORT_RULE: &str = "\n\nImport only project files that the plan lists. Never import a \
project file that is not in the plan (for example a `./SomethingClient` beside a page): write \
that code inside your own file.";

/// Added to the prompt of a task that waits for earlier files.
const STAGED_RULE: &str = "\n\nThe project files that are already written are attached below as \
prerequisite outputs. Import from them. Use exactly the names, fields, signatures and literal \
values that they declare, and do not declare again a type that they export.";

/// The profile a staged `slim` plan is built with when the engine stages it
/// (`engstage` on) or tidies its manifest (`engtidy` on): `base` with its
/// staging and its `tidy_manifest` hook edited so that each bench switch
/// keeps its meaning. `None` when nothing needs an edit, so the engine
/// detects the profile as before.
///
/// | switch off    | bench path (`engstage`/`engtidy` off) | engine path (`engstage`/`engtidy` on) |
/// |---------------|-------------------------------|---------------------------------------------|
/// | `engstage`    | (this path)                   | the bench `stage_plan` runs instead         |
/// | `noprime`     | `stage_plan` keeps the primes | `finish_staging` puts back the primes of the unstaged plan |
/// | `nextrule`    | no `NEXT_RULE`                | the `NEXT_RULE` path rule is empty          |
/// | `testrule`    | no `TEST_RULE`                | the `TEST_RULE` path rule is empty          |
/// | `typeswin`    | no `TYPES_WIN_RULE`           | `types_win_rule` is empty                   |
/// | `seed`        | the seed waits for `lib` too  | the Data layer (the seed) is a Consumer; the seed notes run in `engine_tidy_rest` |
/// | `engtidy`     | (this path)                   | the bench `tidy_manifest` and seed notes run instead |
/// | `onetype`     | `tidy_manifest` skips it      | no `one_type_owner`; `engine_tidy_rest` runs `first_type_owner_wins` (with `oneowner`) |
/// | `oneowner`    | `tidy_manifest` skips it      | no `one_type_owner`; `engine_tidy_rest` runs `types_files_own_types` (with `onetype`) |
/// | `clientpages` | `tidy_manifest` skips it      | the `tidy_manifest` hook is `next_dyn_route` only |
/// | `dynroute`    | `tidy_manifest` skips it      | the `tidy_manifest` hook is `next_client_pages` only |
/// | `seedcompact` | no seed note, no date         | `data_note` is empty, `today` is `None`     |
/// | `seeddates`   | no date                       | `today` is `None`                           |
///
/// The CSS split has its own switch: with `engcss` on (`csssplit` on),
/// `PlanOptions::split` splits the style sheet and renders the joined file;
/// off, the bench's `split_css` and `css_imports` do. With `csssplit` off,
/// neither runs.
fn slim_profile(base: &'static Profile, staged: bool) -> Option<&'static Profile> {
    slim_profile_with(base, staged, step_on)
}

/// `slim_profile` with the switches read from `on`.
fn slim_profile_with(
    base: &'static Profile,
    staged: bool,
    on: impl Fn(&str) -> bool,
) -> Option<&'static Profile> {
    // The date does not change the profile.
    let options = slim_plan_options_with(staged, &on, (1970, 1, 1));
    let hook = tidy_hook(&on).filter(|_| base.name == slimtoken::profile::NEXTJS.name);
    let tidy_edit = options.tidy && (hook.is_some() || !on("seedcompact"));
    if !options.staged && !tidy_edit {
        return None;
    }
    let mut staging = base.staging;
    if options.staged {
        let layers: Vec<(&'static str, Layer)> = staging
            .layers
            .iter()
            .map(|&(glob, layer)| match layer {
                Layer::Data if !on("seed") => (glob, Layer::Consumer),
                _ => (glob, layer),
            })
            .collect();
        let path_rules: Vec<(&'static str, &'static str)> = staging
            .path_rules
            .iter()
            .map(|&(glob, rule)| {
                let off = (rule == NEXT_RULE && !on("nextrule"))
                    || (rule == TEST_RULE && !on("testrule"));
                (glob, if off { "" } else { rule })
            })
            .collect();
        staging = Staging {
            layers: Box::leak(layers.into_boxed_slice()),
            path_rules: Box::leak(path_rules.into_boxed_slice()),
            types_win_rule: if on("typeswin") {
                staging.types_win_rule
            } else {
                ""
            },
            ..staging
        };
    }
    let mut tidy_manifest = base.tidy_manifest;
    if options.tidy {
        if !on("seedcompact") {
            staging.data_note = "";
        }
        if let Some(hook) = hook {
            tidy_manifest = hook;
        }
    }
    let profile = Profile {
        staging,
        tidy_manifest,
        ..*base
    };
    Some(Box::leak(Box::new(profile)))
}

/// The NEXTJS `tidy_manifest` hook for `clientpages` and `dynroute`;
/// `None` with both on (the profile's own hook).
fn tidy_hook(on: &impl Fn(&str) -> bool) -> Option<fn(&mut Manifest)> {
    fn none(_: &mut Manifest) {}
    match (on("clientpages"), on("dynroute")) {
        (true, true) => None,
        (true, false) => Some(slimtoken::profile::next_client_pages),
        (false, true) => Some(slimtoken::profile::next_dyn_route),
        (false, false) => Some(none),
    }
}

/// Finish staging the plan of a staged run: with `options.staged` the
/// engine staged it and only `noprime` off needs the primes of the unstaged
/// plan back; without, the bench's `stage_plan` stages it.
fn finish_staging(
    plan: &mut Plan,
    objective: &str,
    manifest: &slimtoken::architect::Manifest,
    max_tasks: usize,
    profile: Option<&'static Profile>,
    options: &PlanOptions,
    on: impl Fn(&str) -> bool,
) -> Result<()> {
    if !options.staged {
        stage_plan_with(plan, on);
        return Ok(());
    }
    if !on("noprime") {
        let flat = PlanOptions {
            staged: false,
            ..*options
        };
        plan.cache_primes =
            plan_from_manifest(objective, manifest, max_tasks, profile, &flat)?.cache_primes;
    }
    Ok(())
}

/// Order the wave in three stages, so that later files are written against
/// the real text of the files they build on, not only against the plan:
/// the shared type files first, then the other `lib` modules (they see the
/// types), then pages, components and tests (they see the types and `lib`).
/// Files in one stage are still written at the same time. With `engstage`
/// on, the engine's `PlanOptions::staged` does this instead.
fn stage_plan_with(plan: &mut slimtoken::dag::Plan, step_on: impl Fn(&str) -> bool) {
    let is_code = |path: &str| path.ends_with(".ts") || path.ends_with(".tsx");
    let is_test = |path: &str| path.contains(".test.") || path.contains(".spec.");
    let is_types = |path: &str| {
        let name = path.rsplit('/').next().unwrap_or(path);
        name == "types.ts" || name.ends_with(".d.ts") || path.contains("/types/")
    };
    // The seed data is long and nothing needs its text: no file waits for
    // it, and (with `seed`) it waits for the types only.
    let is_seed = |path: &str| {
        path.rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with("seed."))
    };
    let is_lib = |path: &str| {
        (path.starts_with("lib/") || path.starts_with("src/lib/"))
            && !is_seed(path)
            && path.ends_with(".ts")
            && !is_test(path)
            && !is_types(path)
    };
    let written: Vec<(String, String)> = plan
        .tasks
        .iter()
        .filter(|task| task.static_output.is_none() && !task.internal && is_code(&task.title))
        .map(|task| (task.id.clone(), task.title.clone()))
        .collect();
    let types: Vec<String> = written
        .iter()
        .filter(|(_, path)| is_types(path))
        .map(|(id, _)| id.clone())
        .collect();
    let lib: Vec<String> = written
        .iter()
        .filter(|(_, path)| is_lib(path))
        .map(|(id, _)| id.clone())
        .collect();
    // `qcss`: pages and components also wait for the parts of the style
    // sheet (as `PlanOptions::consumers_see_css` on the engine path).
    let css_parts: Vec<String> = if step_on("qcss") {
        plan.tasks
            .iter()
            .filter(|task| task.static_output.is_none() && !task.internal)
            .filter(|task| task.title.contains("styles/part") && task.title.ends_with(".css"))
            .map(|task| task.id.clone())
            .collect()
    } else {
        Vec::new()
    };
    let sees_css = |path: &str| {
        (path.starts_with("app/") || path.starts_with("components/"))
            && path.ends_with(".tsx")
            && !is_test(path)
    };
    // The first stage has two or three files and it warms the prefix cache
    // for the stages after it, so separate cache-prime requests before the
    // wave only add their wait (about 2 s).
    if step_on("noprime") {
        plan.cache_primes.clear();
    }
    for task in &mut plan.tasks {
        let path = task.title.as_str();
        if task.static_output.is_some() || task.internal || !is_code(path) {
            continue;
        }
        task.prompt.push_str(IMPORT_RULE);
        if path.ends_with(".tsx") && !is_test(path) && step_on("nextrule") {
            task.prompt.push_str(NEXT_RULE);
        }
        if is_test(path) && step_on("testrule") {
            task.prompt.push_str(TEST_RULE);
        }
        if is_types(path) {
            continue;
        }
        let mut wanted = types.clone();
        // The seed data needs the types only. It starts with the `lib`
        // stage and has the time of two stages for its length.
        let early_seed = is_seed(path) && step_on("seed");
        // (Tested: pages that start before the `lib` code is written save
        // 2 s and then fail the build in 4 of 6 runs.)
        if !is_lib(path) && !early_seed {
            wanted.extend(lib.iter().cloned());
        }
        if sees_css(path) {
            wanted.extend(css_parts.iter().cloned());
        }
        let before = task.deps.len();
        for id in wanted {
            if id != task.id && !task.deps.contains(&id) {
                task.deps.push(id);
            }
        }
        if task.deps.len() > before {
            // `prefix`: the deps come before this task's tail.
            if task.prefix_split.is_some() {
                task.prompt
                    .push_str(&STAGED_RULE.replace("attached below", "attached above"));
            } else {
                task.prompt.push_str(STAGED_RULE);
            }
            if is_lib(path) && step_on("typeswin") {
                task.prompt.push_str(TYPES_WIN_RULE);
            }
        }
    }
}

/// One quality check: a named command that runs in an app directory.
#[derive(Debug, Clone)]
pub struct CheckStep {
    pub name: String,
    pub command: String,
}

/// A shell command, with no window and no input.
pub fn shell(command: &str) -> std::process::Command {
    if cfg!(windows) {
        let mut process = std::process::Command::new("cmd");
        process.arg("/C").arg(command);
        process
    } else {
        let mut process = std::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    }
}

/// Run the check steps in order in an arm's output directory. A step that
/// fails does not stop the later steps, except a failed install: without
/// dependencies no later step can pass.
pub async fn run_checks(arm: Arm, dir: PathBuf, steps: Vec<CheckStep>, emitter: Emitter) {
    let mut install_failed = false;
    for step in steps {
        if install_failed {
            emitter.emit(Ev::Check {
                arm,
                name: step.name,
                state: CheckState::Fail,
                seconds: 0.0,
                tail: "not run: install failed".to_string(),
            });
            continue;
        }
        emitter.emit(Ev::Check {
            arm,
            name: step.name.clone(),
            state: CheckState::Running,
            seconds: 0.0,
            tail: String::new(),
        });
        let started = Instant::now();
        let mut process = tokio::process::Command::from(shell(&step.command));
        process
            .current_dir(&dir)
            .env("CI", "1")
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(900), process.output()).await;
        let (pass, tail) = match outcome {
            Ok(Ok(output)) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                (output.status.success(), summary_line(&text))
            }
            Ok(Err(error)) => (false, format!("could not start: {error}")),
            Err(_) => (false, "timed out after 15 minutes".to_string()),
        };
        if !pass && step.name == "install" {
            install_failed = true;
        }
        emitter.emit(Ev::Check {
            arm,
            name: step.name,
            state: if pass {
                CheckState::Pass
            } else {
                CheckState::Fail
            },
            seconds: started.elapsed().as_secs_f64(),
            tail,
        });
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// The most useful line of a command's output: the test totals when there
/// are any (Vitest, Jest, pytest), else the first error, else the last line.
fn summary_line(text: &str) -> String {
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let totals = lines.iter().rev().find(|line| {
        let lower = line.to_lowercase();
        (lower.starts_with("tests") || lower.contains(" passed") || lower.contains(" failed"))
            && lower.chars().any(|ch| ch.is_ascii_digit())
            && (lower.contains("passed") || lower.contains("failed"))
    });
    let error = lines.iter().find(|line| {
        let lower = line.to_lowercase();
        lower.contains("error") && !lower.starts_with("npm warn")
    });
    let chosen = totals.or(error).or(lines.last()).copied().unwrap_or("");
    chosen
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(140)
        .collect()
}

#[cfg(test)]
mod tests {
    /// The switches as a run sees them with neither `SHOWDOWN_ON` nor
    /// `SHOWDOWN_OFF` set: on unless [`DEFAULT_OFF`] lists them.
    fn default_on(name: &str) -> bool {
        !DEFAULT_OFF.contains(&name)
    }

    use super::*;

    #[test]
    fn default_off_steps_need_showdown_on() {
        assert!(step_on_with("wide", "", ""));
        assert!(!step_on_with("wide", "", "a, wide"));
        assert!(!step_on_with("examples", "", ""));
        assert!(step_on_with("examples", "x,examples", ""));
        assert!(
            !step_on_with("examples", "examples", "examples"),
            "OFF wins"
        );
        assert!(DEFAULT_OFF.contains(&"examples"));
        for name in [
            "qrules",
            "qobjective",
            "qliteral",
            "qcontract",
            "qwiring",
            "qcss",
            "prefix",
            "qlint",
            "pagelocal",
            "qsemantic",
            "qlint2",
            "typesfirst",
            "planrefs",
            "planreview",
            "layered",
            "corewhole",
        ] {
            assert!(DEFAULT_OFF.contains(&name), "{name} is off by default");
            assert!(step_on_with(name, name, ""));
        }
        // `typesfirst` is measured and recommended; the other coherence
        // steps are not measured yet or cost planning time without a gain.
        assert!(RECOMMENDED_ON.contains(&"typesfirst"));
        for name in ["shellfix", "nocut", "filebudget"] {
            assert!(RECOMMENDED_ON.contains(&name), "{name} is recommended");
        }
        for name in [
            "planrefs",
            "planreview",
            "layered",
            "corewhole",
            "skelcompact",
            "inwave",
            "inwavelint",
        ] {
            assert!(!RECOMMENDED_ON.contains(&name), "{name} is not recommended");
        }
        for name in ["traceall", "prerendered", "gatereason", "typedecl"] {
            assert!(!DEFAULT_OFF.contains(&name), "{name} is on by default");
        }
    }

    #[test]
    fn transport_steps_are_off_by_default_and_not_recommended() {
        for name in ["prewarm", "hedge", "h2"] {
            assert!(DEFAULT_OFF.contains(&name), "{name} is off by default");
            assert_eq!(RECOMMENDED_ON.contains(&name), name == "hedge", "{name}");
            assert!(!step_on_with(name, "", ""));
            assert!(step_on_with(name, name, ""));
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tap = || {
            TapClient::new(
                "http://127.0.0.1:9/v1",
                "m",
                None,
                Arm::Slim,
                Emitter::new(tx.clone()),
            )
            .unwrap()
        };
        assert!(!slim_transport(tap(), |_| false).unwrap().hedged());
        assert!(slim_transport(tap(), |name| name == "hedge")
            .unwrap()
            .hedged());
        assert!(!slim_transport(tap(), |name| name == "h2").unwrap().hedged());
    }

    #[test]
    fn planpipe_is_off_by_default_and_not_recommended() {
        assert!(DEFAULT_OFF.contains(&"planpipe"));
        assert!(!RECOMMENDED_ON.contains(&"planpipe"));
        assert!(!step_on_with("planpipe", "", ""));
        assert!(step_on_with("planpipe", "planpipe", ""));
    }

    #[test]
    fn skelcompact_is_off_by_default_and_not_recommended() {
        assert!(DEFAULT_OFF.contains(&"skelcompact"));
        assert!(!RECOMMENDED_ON.contains(&"skelcompact"));
        assert!(!step_on_with("skelcompact", "", ""));
        assert!(step_on_with("skelcompact", "skelcompact", ""));
        assert!(!step_on_with("skelcompact", "skelcompact", "skelcompact"));
    }

    #[test]
    fn inwave_is_off_by_default_and_needs_the_checker_and_the_engine_loop() {
        assert!(DEFAULT_OFF.contains(&"inwave"));
        assert!(!RECOMMENDED_ON.contains(&"inwave"));
        assert!(!step_on_with("inwave", "", ""));
        assert!(!in_wave_on(|name| step_on_with(name, "", "")));
        assert!(in_wave_on(|name| step_on_with(name, "inwave", "")));
        assert!(!in_wave_on(|name| step_on_with(name, "inwave", "pchk")));
        assert!(!in_wave_on(|name| step_on_with(
            name,
            "inwave",
            "engrepair"
        )));
    }

    #[test]
    fn shell_switches_are_off_by_default_and_only_shellfix_recommended() {
        for name in ["shellfix", "shellrule", "inwavelint"] {
            assert!(DEFAULT_OFF.contains(&name), "{name}");
            assert_eq!(RECOMMENDED_ON.contains(&name), name == "shellfix", "{name}");
            assert!(!step_on_with(name, "", ""), "{name}");
            assert!(step_on_with(name, name, ""), "{name}");
        }
        let off = slim_plan_options_with(true, |name| step_on_with(name, "", ""), TODAY);
        assert!(!off.shell_rule);
        let on = slim_plan_options_with(true, |name| step_on_with(name, "shellrule", ""), TODAY);
        assert!(on.shell_rule);
    }

    /// `typesplan`: off by default, recommended, and it needs both a staged
    /// wave and `typesfirst` (the plan must hold the types as code).
    #[test]
    fn typesplan_is_off_by_default_and_needs_typesfirst_and_staging() {
        assert!(DEFAULT_OFF.contains(&"typesplan"));
        for name in ["typesplan", "jointedit", "testedit", "specwave", "hedge"] {
            assert!(DEFAULT_OFF.contains(&name), "{name}");
            assert!(RECOMMENDED_ON.contains(&name), "{name}");
        }
        for name in ["fencestop", "speedrace", "pagefork"] {
            assert!(!RECOMMENDED_ON.contains(&name), "{name}");
        }
        let with = |list: &'static str| move |name: &str| step_on_with(name, list, "");
        assert!(!slim_plan_options_with(true, with(""), TODAY).types_from_plan);
        assert!(!slim_plan_options_with(true, with("typesplan"), TODAY).types_from_plan);
        assert!(slim_plan_options_with(true, with("typesplan,typesfirst"), TODAY).types_from_plan);
        assert!(
            !slim_plan_options_with(false, with("typesplan,typesfirst"), TODAY).types_from_plan
        );
    }

    #[test]
    fn nocut_switches_are_off_by_default_and_recommended() {
        for name in ["nocut", "filebudget"] {
            assert!(DEFAULT_OFF.contains(&name), "{name}");
            assert!(RECOMMENDED_ON.contains(&name), "{name}");
            assert!(!step_on_with(name, "", ""), "{name}");
        }
        let mut off = SchedulerConfig::default();
        no_cut(&mut off, |name| step_on_with(name, "", ""));
        assert_eq!(off.continue_truncated, None);
        assert_eq!(off.file_min_tokens, None);
        let mut on = SchedulerConfig::default();
        no_cut(&mut on, |name| step_on_with(name, "nocut,filebudget", ""));
        assert_eq!(
            on.continue_truncated,
            Some(slimtoken::continuation::DEFAULT_CAP)
        );
        assert!(on.file_min_tokens.is_some());
    }

    #[test]
    fn splitpage_is_off_by_default_and_needs_pagelocal() {
        assert!(DEFAULT_OFF.contains(&"splitpage"));
        assert!(!RECOMMENDED_ON.contains(&"splitpage"));
        assert!(!step_on_with("splitpage", "", ""));
        assert!(!split_page_on(|name| step_on_with(name, "", "")));
        assert!(!split_page_on(|name| step_on_with(name, "splitpage", "")));
        assert!(split_page_on(|name| step_on_with(
            name,
            "splitpage,pagelocal",
            ""
        )));
        assert!(!split_page_on(|name| step_on_with(
            name,
            "splitpage,pagelocal",
            "splitpage"
        )));
    }

    /// `skelcompact`: no event with the switch off; on, one `skelcompact`
    /// tool event with the report. The compact prompt keeps the line the tap
    /// timeline finds the skeleton by.
    #[tokio::test]
    async fn skelcompact_event_only_with_the_switch_on() {
        assert!(slimtoken::skelcompact::compact_prompt("obj", "lib/x.ts")
            .contains(crate::tap::SKELETON_PROMPT));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emitter = Emitter::new(tx);
        let tap = TapClient::new(
            "http://127.0.0.1:9/v1",
            "m",
            None,
            Arm::Slim,
            emitter.clone(),
        )
        .unwrap();
        emit_skel_compact(&emitter, Arm::Slim, &tap, None);
        assert!(rx.try_recv().is_err(), "off: no event");
        let sink = slimtoken::skelcompact::SkelSink::default();
        *sink.lock().unwrap() = Some(slimtoken::skelcompact::SkelReport {
            format: slimtoken::skelcompact::SkelFormat::Lines,
            call_ms: 2_400,
            skeleton_ms: 2_410,
            chars: 1_900,
            output_tokens: 480,
            files: 29,
            reason: String::new(),
        });
        emit_skel_compact(&emitter, Arm::Slim, &tap, Some(&sink));
        let event = format!("{:?}", rx.try_recv().unwrap());
        assert!(event.contains("skelcompact"), "{event}");
        assert!(event.contains("format=lines"), "{event}");
        assert!(event.contains("output_tokens=480"), "{event}");
    }

    /// The planning timeline: the skeleton and the fragments are told apart
    /// by their prompts (a retried skeleton counts its last close; a salvage
    /// call counts for neither), the first file stream is the wave.
    #[tokio::test]
    async fn tap_timeline_marks_the_planning_steps() {
        use slimtoken::{CompletionRequest, ModelClient};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tap = TapClient::new(
            "http://127.0.0.1:9/v1",
            "m",
            None,
            Arm::Slim,
            Emitter::new(tx),
        )
        .unwrap();
        assert_eq!(tap.timeline(), crate::tap::PlanTimeline::default());
        let request = |user: &str| CompletionRequest {
            system: String::new(),
            shared_context: None,
            user: user.to_string(),
            max_tokens: 1,
            temperature: 0.0,
            reasoning_effort: None,
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        };
        let skeleton = format!("Return {} please", crate::tap::SKELETON_PROMPT);
        let fragment = format!("{} for this objective.", crate::tap::FRAGMENT_PROMPT);
        // Port 9 refuses: each call fails fast, but still opens and closes.
        let _ = tap.complete(request(&skeleton)).await;
        let first_attempt = tap.timeline();
        assert!(first_attempt.skeleton_ms.is_some());
        assert!(first_attempt.first_fragment_start_ms.is_none());
        // A salvage call (another architect call) is neither step.
        let _ = tap.complete(request("repair this JSON")).await;
        assert!(tap.timeline().first_fragment_start_ms.is_none());
        // The retried skeleton: its close is the one that counts.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let _ = tap.complete(request(&skeleton)).await;
        let after_skeleton = tap.timeline();
        assert!(after_skeleton.skeleton_ms > first_attempt.skeleton_ms);
        let _ = tap.complete(request(&fragment)).await;
        assert!(tap.timeline().first_fragment_start_ms.is_some());
        assert!(tap.timeline().wave_first_open_ms.is_none());
        assert!(tap.timeline().planning_done_ms.is_none());
        tap.mark_planning_done();
        let _ = tap.complete(request("Title: lib/a.ts")).await;
        let timeline = tap.timeline();
        assert!(timeline.wave_first_open_ms.is_some());
        assert!(timeline.planning_done_ms.is_some());
        assert!(timeline.planning_done_ms <= timeline.wave_first_open_ms);
        assert_eq!(timeline.skeleton_ms, after_skeleton.skeleton_ms);
    }

    #[test]
    fn qsemantic_sets_the_plan_option_and_rerepair_leaves_it_off() {
        let off = slim_plan_options_with(true, default_on, (2026, 11, 16));
        assert!(!off.semantics);
        let on = |name: &str| name == "qsemantic" || default_on(name);
        assert!(slim_plan_options_with(true, on, (2026, 11, 16)).semantics);
        assert!(slim_plan_options_with(false, on, (2026, 11, 16)).semantics);
        assert_eq!(
            PlanOptions {
                semantics: false,
                ..slim_plan_options_with(true, on, (2026, 11, 16))
            },
            slim_plan_options_with(true, default_on, (2026, 11, 16)),
            "qsemantic changes nothing else"
        );
        let manifest = Manifest {
            files: Vec::new(),
            test_files: Vec::new(),
            readme: false,
        };
        assert!(!rerepair_plan_options_with(&manifest, Path::new("."), true, on).semantics);
    }

    #[test]
    fn examples_add_the_rules_to_the_nextjs_heads_only() {
        let base = &slimtoken::profile::NEXTJS;
        let edited = with_examples(base);
        assert!(edited.edge_rule.starts_with(base.edge_rule));
        assert!(edited.edge_rule.ends_with(EXAMPLE_EDGE_RULE));
        assert!(edited
            .module_head
            .ends_with(&format!("\n{EXAMPLE_MODULE_RULE}\n")));
        assert!(edited
            .module_head
            .starts_with(base.module_head.trim_end_matches('\n')));
        assert!(
            edited.test_head.contains(EXAMPLE_TEST_RULE),
            "test rule placed"
        );
        assert!(edited.test_head.contains("{silent}") && edited.test_head.contains("{all}"));
        assert_eq!(edited.name, base.name);
        let python = &slimtoken::profile::PYTHON;
        assert!(std::ptr::eq(with_examples(python), python));
    }

    #[test]
    fn typedecl_rides_on_tidy_and_rerepair_leaves_it_off() {
        let on = default_on;
        assert!(slim_plan_options_with(true, on, TODAY).declare_types);
        assert!(!slim_plan_options_with(false, on, TODAY).declare_types);
        let off = |name: &str| default_on(name) && name != "typedecl";
        assert!(!slim_plan_options_with(true, off, TODAY).declare_types);
        let no_tidy = |name: &str| default_on(name) && name != "engtidy";
        assert!(!slim_plan_options_with(true, no_tidy, TODAY).declare_types);
        let app = std::env::temp_dir().join(format!("showdown-typedecl-{}", std::process::id()));
        let options = rerepair_plan_options_with(&small_manifest(4, 1), &app, true, on);
        assert!(!options.declare_types);
    }

    #[test]
    fn typedecl_gives_the_same_manifest_on_the_bench_and_the_engine_path() {
        let text = r#"{"files":[
          {"path":"lib/types.ts","purpose":"types","exports":["export interface Task { id: string }"]},
          {"path":"lib/storage.ts","purpose":"storage","exports":["export function loadState(): AppState  // {tasks: Task[], settings: Settings}"]},
          {"path":"components/Column.tsx","purpose":"column","exports":["export default function Column(props: {status: TaskStatus})"]}
        ],"test_files":[],"readme":false}"#;
        let base = &slimtoken::profile::NEXTJS;
        let engine_off = |name: &str| default_on(name) && name != "engtidy";
        let mut bench: Manifest = serde_json::from_str(text).unwrap();
        prepare_manifest_with(&mut bench, true, base, engine_off, TODAY);
        let mut engine: Manifest = serde_json::from_str(text).unwrap();
        let on = default_on;
        prepare_manifest_with(&mut engine, true, base, on, TODAY);
        apply_manifest_passes(&mut engine, base, &slim_plan_options_with(true, on, TODAY));
        let lines = |manifest: &Manifest| -> Vec<String> {
            manifest
                .files
                .iter()
                .flat_map(|file| file.exports.iter().cloned())
                .collect()
        };
        assert_eq!(lines(&bench), lines(&engine));
        assert!(lines(&bench)
            .iter()
            .any(|line| line.starts_with("export interface AppState { tasks: Task[]")));
        // Switched off, neither path adds a line.
        let typedecl_off = |name: &str| default_on(name) && name != "typedecl" && name != "engtidy";
        let mut plain: Manifest = serde_json::from_str(text).unwrap();
        prepare_manifest_with(&mut plain, true, base, typedecl_off, TODAY);
        let original: Manifest = serde_json::from_str(text).unwrap();
        assert_eq!(lines(&plain), lines(&original));
    }

    #[test]
    fn tidy_manifest_gives_each_type_one_owner_and_makes_param_pages_client() {
        let text = r#"{"files":[
          {"path":"lib/types.ts","purpose":"types","exports":["export interface Task { id: string }"]},
          {"path":"lib/tasks.ts","purpose":"tasks.","exports":["export interface Task  // other","export function all(): Task[]"]},
          {"path":"app/tasks/[id]/page.tsx","purpose":"detail","exports":["export default async function TaskPage({ params }: { params: Promise<{ id: string }> })"]}
        ]}"#;
        let mut manifest: slimtoken::architect::Manifest = serde_json::from_str(text).unwrap();
        tidy_manifest_with(&mut manifest, &|_: &str| true);
        assert_eq!(manifest.files[0].exports.len(), 1);
        assert_eq!(manifest.files[1].exports, ["export function all(): Task[]"]);
        assert!(manifest.files[1]
            .purpose
            .contains("Imports Task from `lib/types.ts`"));
        let page = &manifest.files[2].exports[0];
        assert!(
            page.starts_with("export default function TaskPage()") && page.contains("useParams")
        );
    }

    #[tokio::test]
    async fn engine_and_bench_planning_agree_on_a_replayed_manifest() {
        let text = r#"{"files":[
          {"path":"lib/types.ts","purpose":"types","exports":["export interface Task { id: string }"]},
          {"path":"lib/tasks.ts","purpose":"tasks.","exports":["export interface Task  // other","export function all(): Task[]"]},
          {"path":"lib/seed.ts","purpose":"demo records","exports":["export const seed: Task[]"]},
          {"path":"app/tasks/[id]/page.tsx","purpose":"detail","exports":["export default async function TaskPage({ params }: { params: Promise<{ id: string }> })"]},
          {"path":"app/page.tsx","purpose":"home","exports":["export default function Home()"]}
        ],"test_files":[{"path":"tests/app.test.ts","purpose":"all tests","exports":[]}],"readme":false}"#;
        let saved: slimtoken::architect::Manifest = serde_json::from_str(text).unwrap();
        let client = slimtoken::client::MockClient::new(std::time::Duration::ZERO);
        for staged in [false, true] {
            let config = LiveConfig {
                objective: "Build a Next.js task tracker with vitest tests.".into(),
                base_url: String::new(),
                model: String::new(),
                api_key: None,
                preset: None,
                effort: None,
                max_tasks: 40,
                single_max_tokens: 1_000,
                manifest: None,
                out_dir: PathBuf::new(),
                gate_cmd: None,
                staged,
                repair_rounds: 0,
            };
            let architect = ArchitectOptions::default();
            let profile = slim_profile(Profile::detect(&config.objective), staged);
            let (bench_manifest, bench_plan) = plan_slim(
                &client,
                &config,
                &architect,
                Some(saved.clone()),
                false,
                profile,
                TODAY,
            )
            .await
            .unwrap();
            let (engine_manifest, engine_plan) = plan_slim(
                &client,
                &config,
                &architect,
                Some(saved.clone()),
                true,
                profile,
                TODAY,
            )
            .await
            .unwrap();
            assert_eq!(
                json(&bench_manifest),
                json(&engine_manifest),
                "staged {staged}"
            );
            assert_eq!(json(&bench_plan), json(&engine_plan), "staged {staged}");
            if staged {
                // The bench steps ran: the duplicate type declaration is gone.
                assert_eq!(
                    engine_manifest.files[1].exports,
                    ["export function all(): Task[]"]
                );
            } else {
                assert_eq!(json(&engine_manifest), json(&saved));
            }
        }
    }

    /// Answers every request with one fixed manifest and keeps the prompts,
    /// so both planning paths can be compared on a generated manifest.
    struct FixedManifest {
        text: String,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    #[slimtoken::async_trait]
    impl ModelClient for FixedManifest {
        async fn complete(
            &self,
            request: slimtoken::CompletionRequest,
        ) -> Result<slimtoken::Completion> {
            let prompt = format!(
                "{}\n{}\n{}",
                request.system,
                request.shared_context.unwrap_or_default(),
                request.user
            );
            self.prompts.lock().unwrap().push(prompt);
            Ok(slimtoken::Completion {
                text: self.text.clone(),
                provider: None,
                input_tokens: 10,
                output_tokens: 10,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".to_string()),
                usage_is_estimated: true,
                elapsed: std::time::Duration::ZERO,
                time_to_first_token: None,
            })
        }
    }

    #[tokio::test]
    async fn engine_and_bench_planning_agree_on_a_generated_manifest() {
        // The engine passes the detected profile to the architect, the bench
        // passes none (detected inside): the prompts and plans must match.
        let text = r#"{"files":[
          {"path":"lib/tasks.ts","purpose":"tasks.","exports":["export function all(): string[]"]},
          {"path":"app/page.tsx","purpose":"home","exports":["export default function Home()"]}
        ],"test_files":[{"path":"tests/app.test.ts","purpose":"all tests","exports":[]}],"readme":false}"#;
        let config = LiveConfig {
            objective: "Build a Next.js task tracker with vitest tests.".into(),
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            preset: None,
            effort: None,
            max_tasks: 40,
            single_max_tokens: 1_000,
            manifest: None,
            out_dir: PathBuf::new(),
            gate_cmd: None,
            staged: false,
            repair_rounds: 0,
        };
        // Single shot: one request whose answer is the manifest as written.
        let architect = ArchitectOptions {
            force_split: false,
            ..ArchitectOptions::default()
        };
        let run = |engine: bool| {
            let config = &config;
            let architect = &architect;
            async move {
                let client = FixedManifest {
                    text: text.to_string(),
                    prompts: std::sync::Mutex::new(Vec::new()),
                };
                let (manifest, plan) =
                    plan_slim(&client, config, architect, None, engine, None, TODAY)
                        .await
                        .unwrap();
                let prompts = client.prompts.into_inner().unwrap();
                (json(&manifest), json(&plan), prompts)
            }
        };
        let bench = run(false).await;
        let engine = run(true).await;
        assert_eq!(bench.2.len(), 1);
        assert_eq!(bench.2, engine.2, "architect prompts");
        assert_eq!(bench.0, engine.0, "manifests");
        assert_eq!(bench.1, engine.1, "plans");
    }

    /// A manifest with `modules` modules and `tests` test files.
    fn small_manifest(modules: usize, tests: usize) -> slimtoken::architect::Manifest {
        let file = |path: String| slimtoken::architect::ManifestFile {
            path,
            purpose: "x".into(),
            exports: vec![],
            uses: Vec::new(),
        };
        slimtoken::architect::Manifest {
            files: (1..=modules)
                .map(|index| file(format!("lib/m{index}.ts")))
                .collect(),
            test_files: (1..=tests)
                .map(|index| file(format!("tests/t{index}.test.ts")))
                .collect(),
            readme: false,
        }
    }

    #[test]
    fn shard_switches_run_the_floor_or_the_padding_never_both() {
        let objective = "Build a Next.js task tracker with vitest tests.";
        for staged in [false, true] {
            for shards in [false, true] {
                for engshards in [false, true] {
                    let on = |name: &str| match name {
                        "shards" => shards,
                        "engshards" => engshards,
                        _ => default_on(name),
                    };
                    let label = format!("staged {staged} shards {shards} engshards {engshards}");
                    let options = slim_plan_options_with(staged, on, TODAY);
                    let mut padded = small_manifest(8, 1);
                    // `prepare_manifest_with` pads only a staged run.
                    if staged {
                        spread_tests_with(&mut padded, on);
                    }
                    let floor = options.min_test_shards > 0;
                    let pad = padded.test_files.len() > 1;
                    assert_eq!(floor, staged && shards && engshards, "{label}");
                    assert_eq!(pad, staged && shards && !engshards, "{label}");
                    assert!(!(floor && pad), "{label}");
                    assert_eq!(options.staged, staged, "{label}");
                    if floor {
                        assert_eq!(options.min_test_shards, TEST_FILES);
                    } else {
                        let unstaged = PlanOptions {
                            staged: false,
                            one_type_owner: false,
                            tidy: false,
                            declare_types: false,
                            today: None,
                            split: false,
                            ..options
                        };
                        assert_eq!(unstaged, PlanOptions::default(), "{label}");
                    }
                    if pad {
                        assert_eq!(padded.test_files.len(), TEST_FILES);
                    }
                }
            }
        }
        // With `shards` on, `engshards` on and off plan the same wave.
        let mut padded = small_manifest(8, 1);
        spread_tests_with(&mut padded, |name| default_on(name) && name != "engshards");
        let floored = plan_from_manifest(
            objective,
            &small_manifest(8, 1),
            40,
            None,
            &slim_plan_options_with(true, |name| default_on(name) && name != "engstage", TODAY),
        )
        .unwrap();
        let padded =
            plan_from_manifest(objective, &padded, 40, None, &PlanOptions::default()).unwrap();
        assert_eq!(json(&floored), json(&padded));
    }

    #[test]
    fn rerepair_plans_a_run_with_its_own_test_file_without_the_floor() {
        let app = std::env::temp_dir().join(format!("showdown-rerepair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&app);
        let manifest = small_manifest(8, 1);
        let objective = "Build a Next.js task tracker with vitest tests.";
        let test_tasks = |options: &PlanOptions| -> Vec<String> {
            plan_from_manifest(objective, &manifest, 200, None, options)
                .unwrap()
                .tasks
                .into_iter()
                .filter(|task| task.title.starts_with("tests/"))
                .map(|task| task.title)
                .collect()
        };
        // No test file on disk: the engine's shards wrote the tests.
        std::fs::create_dir_all(app.join("tests")).unwrap();
        let on = default_on;
        let rerepair = PlanOptions {
            one_type_owner: false,
            tidy: false,
            declare_types: false,
            today: None,
            split: false,
            ..slim_plan_options_with(true, on, TODAY)
        };
        let options = rerepair_plan_options_with(&manifest, &app, true, on);
        assert_eq!(options, rerepair);
        // A run made flat wrote the manifest's one test file.
        std::fs::write(app.join("tests/t1.test.ts"), "test").unwrap();
        let options = rerepair_plan_options_with(&manifest, &app, true, on);
        assert!(options.staged, "still staged");
        assert_eq!(
            options,
            PlanOptions {
                min_test_shards: 0,
                ..rerepair
            }
        );
        let tasks = test_tasks(&options);
        assert_eq!(tasks, ["tests/t1.test.ts"]);
        assert!(tasks.iter().all(|title| app.join(title).is_file()));
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn rerepair_prompts_do_not_depend_on_engtidy() {
        // The single stream's manifest of files: a dynamic route page and
        // seed data, which the tidy passes would rewrite.
        let app =
            std::env::temp_dir().join(format!("showdown-rerepair-tidy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&app);
        for (path, text) in [
            (
                "app/tasks/[id]/page.tsx",
                "export default function Page() {}",
            ),
            ("app/page.tsx", "export default function Home() {}"),
            ("lib/seed.ts", "export const seed = [];"),
            ("lib/types.ts", "export interface Task { id: string }"),
            ("lib/store.ts", "export function all() {}"),
        ] {
            let path = app.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let manifest: Manifest = serde_json::from_str(&manifest_of_files(&app).unwrap()).unwrap();
        let prompts = |on: &dyn Fn(&str) -> bool| -> Vec<(String, String)> {
            let options = rerepair_plan_options_with(&manifest, &app, true, on);
            let profile = slim_profile_with(Profile::detect(STAGE_OBJECTIVE), true, on);
            plan_from_manifest(STAGE_OBJECTIVE, &manifest, 200, profile, &options)
                .unwrap()
                .tasks
                .into_iter()
                .map(|task| (task.title, task.prompt))
                .collect()
        };
        let with = prompts(&default_on);
        let without = prompts(&|name| default_on(name) && name != "engtidy");
        assert_eq!(with, without);
        // The manifest reached the prompts as it is: no seed note, no date,
        // no rewritten page.
        let all = with
            .iter()
            .map(|(_, prompt)| prompt.as_str())
            .collect::<String>();
        assert!(!all.contains("Today is "));
        assert!(!all.contains(SEED_NOTE.trim()));
        assert!(!all.contains("useParams()"));
        let _ = std::fs::remove_dir_all(&app);
    }

    /// The engine's fixture manifest (the root crate's golden tests use it):
    /// a types file, seed data, `lib` modules, pages and a test file.
    fn staging_fixture() -> slimtoken::architect::Manifest {
        serde_json::from_str(include_str!("../../../tests/fixtures/plan-manifest.json")).unwrap()
    }

    const STAGE_OBJECTIVE: &str = "Build a Next.js task tracker with vitest tests.";

    /// The date the root crate's golden manifest was written with.
    const TODAY: (i32, u32, u32) = (2026, 11, 14);

    /// The bench path: an unstaged plan staged by `stage_plan`.
    fn bench_staged(on: impl Fn(&str) -> bool + Copy) -> Plan {
        bench_staged_with(|name: &str| default_on(name) && on(name))
    }

    /// `bench_staged` with the switches of `on` (`engstage` off).
    fn bench_staged_with(on: impl Fn(&str) -> bool + Copy) -> Plan {
        let mut manifest = staging_fixture();
        let off = |name: &str| name != "engstage" && on(name);
        let options = slim_plan_options_with(true, off, TODAY);
        assert!(!options.staged);
        let base = Profile::detect(STAGE_OBJECTIVE);
        prepare_manifest_with(&mut manifest, true, base, off, TODAY);
        let mut plan = plan_from_manifest(STAGE_OBJECTIVE, &manifest, 64, None, &options).unwrap();
        finish_staging(
            &mut plan,
            STAGE_OBJECTIVE,
            &manifest,
            64,
            None,
            &options,
            off,
        )
        .unwrap();
        plan
    }

    /// The engine path: the engine stages the plan (`engstage`).
    fn engine_staged(on: impl Fn(&str) -> bool + Copy) -> Plan {
        let mut manifest = staging_fixture();
        let options = slim_plan_options_with(true, on, TODAY);
        assert!(options.staged);
        let base = Profile::detect(STAGE_OBJECTIVE);
        prepare_manifest_with(&mut manifest, true, base, on, TODAY);
        let profile = slim_profile_with(Profile::detect(STAGE_OBJECTIVE), true, on);
        assert!(profile.is_some());
        let mut plan =
            plan_from_manifest(STAGE_OBJECTIVE, &manifest, 64, profile, &options).unwrap();
        finish_staging(
            &mut plan,
            STAGE_OBJECTIVE,
            &manifest,
            64,
            profile,
            &options,
            on,
        )
        .unwrap();
        plan
    }

    #[test]
    fn engine_staging_matches_stage_plan() {
        let bench = bench_staged(default_on);
        let engine = engine_staged(default_on);
        assert!(
            bench.tasks.iter().any(|task| !task.deps.is_empty()),
            "the fixture is staged"
        );
        for (bench, engine) in bench.tasks.iter().zip(&engine.tasks) {
            assert_eq!(bench.id, engine.id);
            assert_eq!(bench.deps, engine.deps, "deps of {}", bench.id);
            assert_eq!(bench.prompt, engine.prompt, "prompt of {}", bench.id);
        }
        assert_eq!(json(&bench), json(&engine));
        assert!(engine.cache_primes.is_empty());
    }

    #[test]
    fn bench_switches_keep_their_meaning_with_engine_staging() {
        for switch in ["noprime", "nextrule", "testrule", "typeswin", "seed"] {
            let on = move |name: &str| default_on(name) && name != switch;
            let bench = bench_staged(on);
            let engine = engine_staged(on);
            assert_eq!(json(&bench), json(&engine), "{switch} off");
            // Each switch changes the plan.
            assert_ne!(json(&engine), json(&engine_staged(default_on)), "{switch}");
        }
    }

    #[test]
    fn q2_switches_set_the_engine_options_and_agree_on_both_staging_paths() {
        let with = |extra: &'static str| move |name: &str| default_on(name) || name == extra;
        let off = slim_plan_options_with(true, default_on, TODAY);
        assert!(!off.contract && !off.wave_wiring && !off.wiring_rules);
        assert!(!off.consumers_see_css && !off.deps_first);
        assert!(slim_plan_options_with(true, with("qcontract"), TODAY).contract);
        let wiring = slim_plan_options_with(true, with("qwiring"), TODAY);
        assert!(wiring.wave_wiring && wiring.wiring_rules);
        assert!(slim_plan_options_with(true, with("qcss"), TODAY).consumers_see_css);
        assert!(!slim_plan_options_with(false, with("qcss"), TODAY).consumers_see_css);
        assert!(slim_plan_options_with(true, with("prefix"), TODAY).deps_first);
        // `rerepair` keeps them off.
        let all = |name: &str| default_on(name) || name.starts_with('q') || name == "prefix";
        let app = std::env::temp_dir().join("showdown-q2-rerepair-none");
        let rerepair = rerepair_plan_options_with(&staging_fixture(), &app, true, all);
        assert!(!rerepair.contract && !rerepair.wave_wiring && !rerepair.wiring_rules);
        assert!(!rerepair.consumers_see_css && !rerepair.deps_first);

        // `qcss` and `prefix`: the bench's `stage_plan` (`engstage` off) and
        // the engine's staging give the same plan.
        let base = json(&engine_staged(default_on));
        for extra in ["qcss", "prefix"] {
            let on = with(extra);
            let bench = bench_staged_with(on);
            let engine = engine_staged(on);
            for (bench, engine) in bench.tasks.iter().zip(&engine.tasks) {
                assert_eq!(bench.deps, engine.deps, "{extra}: deps of {}", bench.id);
                assert_eq!(
                    bench.prompt, engine.prompt,
                    "{extra}: prompt of {}",
                    bench.id
                );
            }
            assert_eq!(json(&bench), json(&engine), "{extra}");
            assert_ne!(json(&engine), base, "{extra} changes the plan");
        }
        let css = engine_staged(with("qcss"));
        let page = css
            .tasks
            .iter()
            .find(|task| task.title == "app/page.tsx")
            .unwrap();
        assert!(
            page.deps.iter().any(|dep| dep.contains("styles-part")),
            "{:?}",
            page.deps
        );
    }

    #[test]
    fn engstage_off_keeps_the_bench_path() {
        let on = |name: &str| default_on(name) && name != "engstage";
        assert!(!slim_plan_options_with(true, on, TODAY).staged);
        assert!(slim_profile_with(Profile::detect(STAGE_OBJECTIVE), true, on).is_none());
        // A run that is not staged never stages.
        assert!(!slim_plan_options_with(false, default_on, TODAY).staged);
        assert!(slim_profile_with(Profile::detect(STAGE_OBJECTIVE), false, |_| true).is_none());
    }

    /// The manifest of a bench run (`20261114-174355-base-1`), as the
    /// architect wrote it; the root crate's manifest-pass tests use it too.
    fn showdown_fixture() -> Manifest {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/showdown-manifest.json"
        ))
        .unwrap()
    }

    #[test]
    fn engine_tidy_matches_the_golden_manifest() {
        // The bench's steps on the bench manifest give the root crate's
        // golden file, which is the output of the engine's passes.
        let on = default_on;
        let mut manifest = showdown_fixture();
        tidy_manifest_with(&mut manifest, &on);
        seed_notes_with(&mut manifest, &on, TODAY);
        let golden: Manifest = serde_json::from_str(include_str!(
            "../../../tests/fixtures/golden/showdown-tidy.json"
        ))
        .unwrap();
        assert_eq!(json(&manifest), json(&golden));
        assert_ne!(json(&manifest), json(&showdown_fixture()));
    }

    #[test]
    fn engine_split_matches_the_golden_manifest() {
        // The bench's CSS split and rendered style sheet on the bench
        // manifest give the root crate's golden file, which is the output of
        // the engine's `split` option.
        let mut manifest = showdown_fixture();
        split_css(&mut manifest);
        let mut plan = plan_from_manifest(
            STAGE_OBJECTIVE,
            &manifest,
            64,
            None,
            &PlanOptions::default(),
        )
        .unwrap();
        render_css_with(&mut plan, &manifest, |name| {
            default_on(name) && name != "engcss"
        });
        let rendered: std::collections::BTreeMap<String, String> = plan
            .tasks
            .iter()
            .filter(|task| task.title.ends_with(".css"))
            .filter_map(|task| Some((task.title.clone(), task.static_output.clone()?)))
            .collect();
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/golden/showdown-split.json"
        ))
        .unwrap();
        let golden_manifest: Manifest = serde_json::from_value(golden["manifest"].clone()).unwrap();
        assert_eq!(json(&manifest), json(&golden_manifest));
        assert_eq!(serde_json::json!(rendered), golden["rendered"]);
        assert_eq!(rendered.len(), 1);
    }

    #[test]
    fn engine_split_matches_the_bench_at_the_root_and_at_five_parts() {
        // A style sheet at the repo root (parts under `styles/`) and one
        // with 40 classes (6 parts of 7 clamped to 5): the engine's `split`
        // and its `aggregate` hook give the bench's `split_css` and
        // `css_imports`.
        for (path, classes) in [
            ("globals.css", 14),
            ("globals.css", 40),
            ("app/globals.css", 40),
        ] {
            let exports: Vec<String> = (1..=classes).map(|n| format!(".class-{n}")).collect();
            let fixture: Manifest = serde_json::from_value(serde_json::json!({
                "files": [
                    { "path": "app/page.tsx", "purpose": "The home page.", "exports": ["export default function Home()"] },
                    { "path": path, "purpose": "The global style sheet.", "exports": exports },
                ]
            }))
            .unwrap();
            let mut bench = fixture.clone();
            split_css(&mut bench);
            let options = PlanOptions {
                split: true,
                ..PlanOptions::default()
            };
            let mut engine = fixture.clone();
            apply_manifest_passes(&mut engine, &slimtoken::profile::NEXTJS, &options);
            assert_eq!(json(&engine), json(&bench), "{path}, {classes}");
            let plan = plan_from_manifest(
                STAGE_OBJECTIVE,
                &fixture,
                64,
                Some(&slimtoken::profile::NEXTJS),
                &options,
            )
            .unwrap();
            let rendered: Vec<(String, String)> = plan
                .tasks
                .iter()
                .filter_map(|task| Some((task.title.clone(), task.static_output.clone()?)))
                .collect();
            let expected: Vec<(String, String)> = css_imports(&bench)
                .into_iter()
                .map(|(path, text)| (path, format!("```css\n{}\n```", text.trim_end())))
                .collect();
            assert_eq!(rendered, expected, "{path}, {classes}");
        }
    }

    #[test]
    fn engcss_matches_the_bench_css_steps() {
        // `tidy_path` and then the bench's rendered style sheet, as `slim`
        // does after `finish_staging`.
        let path = |on: &dyn Fn(&str) -> bool| {
            let on = |name: &str| on(name);
            let (manifest, plan) = tidy_path(&showdown_fixture(), true, on);
            let manifest: Manifest = serde_json::from_str(&manifest).unwrap();
            let mut plan: Plan = serde_json::from_str(&plan).unwrap();
            render_css_with(&mut plan, &manifest, on);
            (json(&manifest), json(&plan))
        };
        for switch in ["", "csssplit", "engtidy", "engstage", "noprime"] {
            let engine = path(&|name| default_on(name) && name != switch);
            let bench = path(&|name| default_on(name) && name != switch && name != "engcss");
            assert_eq!(bench.0, engine.0, "manifest, {switch} off");
            assert_eq!(bench.1, engine.1, "plan, {switch} off");
            let split = engine.0.contains("app/styles/part1.css");
            assert_eq!(split, switch != "csssplit", "{switch} off");
            let rendered = engine.1.contains("@import");
            assert_eq!(rendered, switch != "csssplit", "{switch} off");
        }
        let off = |name: &str| default_on(name) && name != "engcss";
        assert!(!slim_plan_options_with(true, off, TODAY).split);
        assert!(slim_plan_options_with(true, default_on, TODAY).split);
        assert!(
            !slim_plan_options_with(true, |name| default_on(name) && name != "csssplit", TODAY)
                .split
        );
        assert!(!slim_plan_options_with(false, default_on, TODAY).split);
    }

    /// A small manifest on which each type pass acts: `lib/seed.ts`
    /// declares `Task`, which `lib/types.ts` owns (`onetype`); `lib/storage.ts`
    /// and `lib/store.ts` both declare `AppState` (`oneowner`), and so do
    /// `lib/store.ts` and the later `lib/seed.ts` with `Filter`, so the seed
    /// loses a type to each pass; two components declare their own `Props`
    /// (kept).
    fn owners_fixture() -> Manifest {
        let file = |path: &str, purpose: &str, exports: &[&str]| serde_json::json!({ "path": path, "purpose": purpose, "exports": exports });
        let files = vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &["export interface Task { id: string; due: string }"],
            ),
            file(
                "lib/storage.ts",
                "Local storage.",
                &[
                    "export interface AppState { tasks: Task[] }",
                    "export function load(): AppState",
                ],
            ),
            file(
                "lib/store.ts",
                "The store.",
                &[
                    "export interface AppState { tasks: Task[]; filter: string }",
                    "export type Filter = 'all' | 'open'",
                    "export function useStore(): AppState",
                ],
            ),
            file(
                "lib/seed.ts",
                "Demo tasks.",
                &[
                    "export interface Task { id: string }",
                    "export type Filter = 'all' | 'done'",
                    "export const seedTasks: Task[]",
                ],
            ),
            file(
                "components/TaskCard.tsx",
                "One task.",
                &[
                    "export interface Props { task: Task }",
                    "export default function TaskCard(props: Props)",
                ],
            ),
            file(
                "components/TaskList.tsx",
                "The tasks.",
                &[
                    "export interface Props { tasks: Task[] }",
                    "export default function TaskList(props: Props)",
                ],
            ),
            file(
                "app/tasks/[id]/page.tsx",
                "One task.",
                &["export default async function TaskPage({ params })"],
            ),
            file(
                "app/page.tsx",
                "Home.",
                &["export default async function Home({ searchParams })  // reads ?filter"],
            ),
        ];
        serde_json::from_value(serde_json::json!({
            "files": files,
            "test_files": [file("tests/store.test.ts", "Store tests.", &[])],
            "readme": false,
        }))
        .unwrap()
    }

    /// The `slim` arm's manifest steps and plan for `manifest`, as
    /// `plan_slim` and the staging after it make them (`engplan` off):
    /// `(manifest, plan)` as JSON.
    fn tidy_path(
        manifest: &Manifest,
        staged: bool,
        on: impl Fn(&str) -> bool + Copy,
    ) -> (String, String) {
        let base = Profile::detect(STAGE_OBJECTIVE);
        let profile = slim_profile_with(base, staged, on);
        let options = slim_plan_options_with(staged, on, TODAY);
        let mut manifest = manifest.clone();
        prepare_manifest_with(&mut manifest, staged, base, on, TODAY);
        let mut plan =
            plan_from_manifest(STAGE_OBJECTIVE, &manifest, 64, profile, &options).unwrap();
        apply_manifest_passes(&mut manifest, profile.unwrap_or(base), &options);
        if staged {
            // With `noprime` off, the engine path plans the tidied manifest
            // again (the passes are idempotent).
            finish_staging(
                &mut plan,
                STAGE_OBJECTIVE,
                &manifest,
                64,
                profile,
                &options,
                on,
            )
            .unwrap();
        }
        (json(&manifest), json(&plan))
    }

    #[test]
    fn engtidy_matches_the_bench_steps_for_each_switch() {
        let switches = [
            "",
            "onetype",
            "oneowner",
            "clientpages",
            "dynroute",
            "seedcompact",
            "seeddates",
            "seed",
            "noprime",
        ];
        for (name, fixture) in [
            ("showdown", showdown_fixture()),
            ("owners", owners_fixture()),
        ] {
            for engstage in [true, false] {
                for switch in switches {
                    let on = move |name: &str| {
                        default_on(name) && name != switch && (engstage || name != "engstage")
                    };
                    let bench = tidy_path(&fixture, true, move |name| {
                        default_on(name) && name != "engtidy" && on(name)
                    });
                    let engine = tidy_path(&fixture, true, on);
                    let label = format!("{name}, engstage {engstage}, {switch} off");
                    assert_eq!(bench.0, engine.0, "manifest, {label}");
                    assert_eq!(bench.1, engine.1, "plan, {label}");
                    let all = tidy_path(&fixture, true, move |name| {
                        default_on(name) && (engstage || name != "engstage")
                    });
                    // Each tidy switch changes the manifest (the showdown
                    // manifest has no type that two `lib` files declare:
                    // there `oneowner` has nothing to do after `onetype`).
                    let idle = ["", "seed", "noprime"].contains(&switch)
                        || (name == "showdown" && switch == "oneowner");
                    if !idle {
                        assert_ne!(engine.0, all.0, "{label}");
                    }
                    if switch == "noprime" {
                        assert_ne!(engine.1, all.1, "{label}: primes");
                    }
                }
            }
        }
        // The owners fixture: the seed loses `Task` to the types file and
        // `Filter` to the store, each with its import note, before the seed
        // note and date (also with the seed out of the Data layer).
        for seed in [true, false] {
            let on = move |name: &str| default_on(name) && (seed || name != "seed");
            let (manifest, _) = tidy_path(&owners_fixture(), true, on);
            let manifest: Manifest = serde_json::from_str(&manifest).unwrap();
            let seed_file = &manifest.files[3];
            assert_eq!(
                seed_file.exports,
                ["export const seedTasks: Task[]"],
                "seed {seed}"
            );
            let purpose = &seed_file.purpose;
            let task = purpose.find("Imports Task from `lib/types.ts`").unwrap();
            let filter = purpose.find("Imports Filter from `lib/store.ts`").unwrap();
            let note = purpose.find(SEED_NOTE).unwrap();
            let date = purpose.find("Today is 2026-11-14").unwrap();
            assert!(
                task < filter && filter < note && note < date,
                "seed {seed}: {purpose}"
            );
            // The store lost `AppState` to the storage file.
            assert!(manifest.files[2]
                .purpose
                .contains("Imports AppState from `lib/storage.ts`"));
            // Components keep their own `Props`.
            assert!(manifest.files[4].exports[0].contains("Props"));
            assert!(manifest.files[5].exports[0].contains("Props"));
        }
    }

    /// The bench's seed note and Next.js rule are the engine's texts (the
    /// looped seed and the `<Link>` navigation rule included).
    #[test]
    fn bench_seed_note_and_next_rule_match_the_engine() {
        let staging = &slimtoken::profile::NEXTJS.staging;
        assert_eq!(SEED_NOTE, staging.data_note);
        assert!(SEED_NOTE.contains("with a loop over the periods"));
        assert!(staging
            .path_rules
            .iter()
            .any(|&(glob, rule)| glob == "**/*.tsx" && rule == NEXT_RULE));
        assert!(NEXT_RULE.contains("never with `<a href>` to an app route"));
    }

    #[test]
    fn seed_note_layer_differs_from_the_bench_only_off_typescript_and_in_types() {
        // The bench notes each file whose name starts with `seed.`; the
        // engine notes the NEXTJS Data layer. The CHANGELOG lists these.
        let staging = &Profile::detect(STAGE_OBJECTIVE).staging;
        for path in [
            "lib/seed.ts",
            "src/lib/seed.tsx",
            "lib/seed.data.ts",
            "lib/seed.test.ts",
        ] {
            assert_eq!(staging.layer_of(path), Some(Layer::Data), "{path}");
        }
        assert_eq!(staging.layer_of("lib/seed.json"), None);
        assert_eq!(staging.layer_of("lib/types/seed.ts"), Some(Layer::Types));
    }

    #[test]
    fn engtidy_off_keeps_the_bench_path() {
        let off = |name: &str| default_on(name) && name != "engtidy";
        let options = slim_plan_options_with(true, off, TODAY);
        assert!(!options.tidy && !options.one_type_owner && options.today.is_none());
        let on = default_on;
        let options = slim_plan_options_with(true, on, TODAY);
        assert!(options.tidy && options.one_type_owner);
        assert_eq!(options.today, Some(TODAY));
        // A run that is not staged keeps the manifest as written.
        let unstaged = slim_plan_options_with(false, on, TODAY);
        assert_eq!(unstaged, PlanOptions::default());
        assert_eq!(
            tidy_path(&showdown_fixture(), false, on).0,
            json(&showdown_fixture())
        );
    }

    #[test]
    fn quality_switches_agree_on_the_bench_and_the_engine_path() {
        let sets: [&'static [&'static str]; 4] = [
            &["qrules"],
            &["qobjective"],
            &["qliteral"],
            &["qrules", "qobjective", "qliteral"],
        ];
        for extra in sets {
            let on = move |name: &str| default_on(name) || extra.contains(&name);
            for (label, fixture) in [
                ("showdown", showdown_fixture()),
                ("owners", owners_fixture()),
            ] {
                let engine = tidy_path(&fixture, true, on);
                let bench = tidy_path(&fixture, true, move |name| name != "engtidy" && on(name));
                assert_eq!(engine.0, bench.0, "manifest, {label}, {extra:?}");
                assert_eq!(engine.1, bench.1, "plan, {label}, {extra:?}");
                let before = tidy_path(&fixture, true, default_on);
                if extra != ["qliteral"] {
                    assert_ne!(engine.1, before.1, "{label}, {extra:?} changes the plan");
                }
            }
        }
        // `qrules`: dates relative to the day the data loads.
        let qrules = |name: &str| default_on(name) || name == "qrules";
        let options = slim_plan_options_with(true, qrules, TODAY);
        assert!(options.quality_rules && options.relative_dates);
        assert_eq!(options.today, None);
        let (manifest, plan) = tidy_path(&showdown_fixture(), true, qrules);
        assert!(!manifest.contains("Today is "));
        assert!(manifest.contains("daysAgo(n)"));
        assert!(!plan.contains("theme-dark"));
        assert!(plan.contains("document.documentElement.dataset.theme"));
        // Off by default: the plan options do not change.
        let options = slim_plan_options_with(true, default_on, TODAY);
        assert!(!options.quality_rules && !options.objective_pages && !options.align_literals);
        assert_eq!(options.today, Some(TODAY));
        // `rerepair` leaves the quality steps and the passes off.
        let all = |name: &str| default_on(name) || name.starts_with('q');
        let app = std::env::temp_dir().join("showdown-quality-rerepair-none");
        let rerepair = rerepair_plan_options_with(&showdown_fixture(), &app, true, all);
        assert!(!rerepair.quality_rules && !rerepair.objective_pages);
        assert!(!rerepair.relative_dates && !rerepair.align_literals);
    }

    /// The bench manifest with each page naming the components it renders
    /// (`renders <X/>`, as `qcontract` plans do): the layout's nav, one
    /// card on two pages (through the board's column), the rest on one page.
    fn pagelocal_fixture() -> Manifest {
        let mut manifest = showdown_fixture();
        for file in &mut manifest.files {
            let extra = match file.path.as_str() {
                "app/layout.tsx" => "; renders <Nav/>",
                "app/page.tsx" => {
                    "; renders <SummaryCards/>, <BurndownChart/>, <StatusChart/>, <ActivityFeed/>"
                }
                "app/projects/page.tsx" => "; renders <ProjectCard/>",
                "app/board/page.tsx" => "; renders <BoardColumn/>",
                "components/BoardColumn.tsx" => "; renders <TaskCard/>",
                "app/tasks/[id]/page.tsx" => "; renders <SubtaskList/>, <CommentList/>",
                "app/calendar/page.tsx" => "; renders <CalendarGrid/> and <TaskCard/>",
                "app/search/page.tsx" => "; renders <Highlight/>",
                _ => continue,
            };
            file.exports[0].push_str(extra);
        }
        manifest
    }

    #[test]
    fn pagelocal_agrees_on_the_bench_and_the_engine_path() {
        let fixture = pagelocal_fixture();
        let sets: [&'static [&'static str]; 4] = [
            &["pagelocal"],
            &["pagelocal", "qobjective"],
            &[
                "pagelocal",
                "qrules",
                "qobjective",
                "qliteral",
                "qcss",
                "prefix",
            ],
            // The target configuration: with the contract and the wiring
            // task (readers, wiring lines and deps).
            &[
                "pagelocal",
                "qrules",
                "qobjective",
                "qliteral",
                "qcontract",
                "qwiring",
                "qcss",
                "prefix",
                "qlint",
            ],
        ];
        for extra in sets {
            let on = move |name: &str| default_on(name) || extra.contains(&name);
            let off = move |name: &str| name != "pagelocal" && on(name);
            let engine = tidy_path(&fixture, true, on);
            // The bench's tidy steps (`engtidy` off) and the bench's staging
            // (`engstage` off) give the same manifest and plan.
            let tidy = tidy_path(&fixture, true, move |name| name != "engtidy" && on(name));
            assert_eq!(engine.0, tidy.0, "manifest, {extra:?}");
            assert_eq!(engine.1, tidy.1, "plan, {extra:?}");
            let stage = tidy_path(&fixture, true, move |name| name != "engstage" && on(name));
            assert_eq!(engine.0, stage.0, "manifest, engstage off, {extra:?}");
            assert_eq!(engine.1, stage.1, "plan, engstage off, {extra:?}");
            // The switch changes the plan; the pages keep their deps.
            let before = tidy_path(&fixture, true, off);
            assert_ne!(engine.1, before.1, "{extra:?} changes the plan");
            let plan: Plan = serde_json::from_str(&engine.1).unwrap();
            let unfolded: Plan = serde_json::from_str(&before.1).unwrap();
            for task in plan
                .tasks
                .iter()
                .filter(|task| task.title.ends_with("page.tsx"))
            {
                let old = unfolded.tasks.iter().find(|old| old.id == task.id).unwrap();
                assert_eq!(task.deps, old.deps, "deps of {}, {extra:?}", task.id);
            }
            let manifest: Manifest = serde_json::from_str(&engine.0).unwrap();
            let paths: Vec<&str> = manifest
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect();
            for gone in ["components/BoardColumn.tsx", "components/CommentList.tsx"] {
                assert!(!paths.contains(&gone), "{gone}, {extra:?}");
            }
            for kept in ["components/Nav.tsx", "components/TaskCard.tsx"] {
                assert!(paths.contains(&kept), "{kept}, {extra:?}");
            }
        }
        // Off by default; `rerepair` keeps it off.
        assert!(!slim_plan_options_with(true, default_on, TODAY).page_local);
        let with = |name: &str| default_on(name) || name == "pagelocal";
        assert!(slim_plan_options_with(true, with, TODAY).page_local);
        assert!(slim_plan_options_with(false, with, TODAY).page_local);
        let app = std::env::temp_dir().join("showdown-pagelocal-rerepair-none");
        assert!(!rerepair_plan_options_with(&fixture, &app, true, with).page_local);
    }

    /// JSON text of a manifest or a plan, for byte comparisons.
    fn json<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn engpipe_is_off_by_default_and_needs_the_engine_plan_and_repair() {
        assert!(DEFAULT_OFF.contains(&"engpipe"));
        assert!(!step_on_with("engpipe", "", ""));
        assert!(step_on_with("engpipe", "engpipe", ""));
        assert!(engpipe_with(|_| true));
        for off in ["engpipe", "engplan", "engrepair", "engcheck"] {
            assert!(!engpipe_with(|name| name != off), "{off} off");
        }
    }

    /// `engpipe` offline: the arm through `pipeline::build` with a mock model
    /// (the run's `TapClient` only takes the feeds) and a gate command that
    /// passes. The plan and manifest are those of the bench's engine path
    /// (`plan_slim` + `finish_plan`), the events come in the bench's order,
    /// and the files and `_meta` are written.
    #[tokio::test]
    async fn engpipe_runs_the_arm_through_the_pipeline() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("showdown-engpipe-{}-{nanos}", std::process::id()));
        let meta_dir = root.join("_meta");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let config = LiveConfig {
            objective: STAGE_OBJECTIVE.into(),
            base_url: "http://127.0.0.1:9".into(),
            model: "mock".into(),
            api_key: None,
            preset: None,
            effort: None,
            max_tasks: 64,
            single_max_tokens: 1_000,
            manifest: None,
            out_dir: root.clone(),
            gate_cmd: Some("echo 0 failed, 3 passed".into()),
            staged: true,
            repair_rounds: 2,
        };
        let architect = ArchitectOptions {
            type_ownership: true,
            ..ArchitectOptions::default()
        };
        let profile = slim_profile(Profile::detect(&config.objective), true);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emitter = Emitter::new(tx);
        let client = Arc::new(
            TapClient::new(
                &config.base_url,
                &config.model,
                None,
                Arm::Slim,
                emitter.clone(),
            )
            .unwrap(),
        );
        let run = SlimRun {
            config: &config,
            emitter: &emitter,
            meta_dir: &meta_dir,
            client,
            profile,
            date: TODAY,
            planning: Instant::now(),
            split: None,
            skel: None,
        };
        let model: Arc<dyn ModelClient> = Arc::new(slimtoken::client::MockClient::new(
            std::time::Duration::ZERO,
        ));
        let out_dir = root.join("slimtoken");
        let ran = slim_pipeline(
            run,
            model,
            &out_dir,
            SchedulerConfig::default(),
            architect.clone(),
            Some(staging_fixture()),
        )
        .await
        .unwrap();

        // The bench's engine path on the same manifest.
        let mock = slimtoken::client::MockClient::new(std::time::Duration::ZERO);
        let (manifest, mut plan) = plan_slim(
            &mock,
            &config,
            &architect,
            Some(staging_fixture()),
            true,
            profile,
            TODAY,
        )
        .await
        .unwrap();
        finish_staging(
            &mut plan,
            &config.objective,
            &manifest,
            config.max_tasks,
            profile,
            &slim_plan_options_with(true, step_on, TODAY),
            step_on,
        )
        .unwrap();
        render_css_with(&mut plan, &manifest, step_on);
        plan.validate().unwrap();
        // After the wave the parts of the style sheet are joined into it.
        let mut joined = plan.clone();
        let mut wave = execute_plan(
            &joined,
            Arc::new(slimtoken::client::MockClient::new(
                std::time::Duration::ZERO,
            )),
            SchedulerConfig::default(),
        )
        .await
        .unwrap();
        let sheets = slimtoken::stylesheet::join_imported_stylesheets(&mut joined, &mut wave);
        assert_eq!(sheets, ["app/globals.css"]);
        // The whole plan (deps, prompts, static outputs), not only the titles.
        assert_eq!(
            serde_json::to_value(&ran).unwrap(),
            serde_json::to_value(&joined).unwrap(),
            "the pipeline ran the bench engine path's plan"
        );
        let saved = std::fs::read_to_string(meta_dir.join("slimtoken-manifest.json")).unwrap();
        assert_eq!(saved, serde_json::to_string_pretty(&manifest).unwrap());
        let report: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(meta_dir.join("slimtoken-report.json")).unwrap(),
        )
        .unwrap();
        let titles = |plan: &Plan| -> Vec<String> {
            let mut titles: Vec<String> =
                plan.tasks.iter().map(|task| task.title.clone()).collect();
            titles.sort();
            titles
        };
        let tasks = report["tasks"].as_object().unwrap();
        let mut reported: Vec<String> = tasks
            .values()
            .map(|task| task["title"].as_str().unwrap().to_string())
            .collect();
        reported.sort();
        assert_eq!(reported, titles(&joined));
        assert!(!reported.iter().any(|title| title.contains("styles/part")));
        assert!(tasks.values().all(|task| task["status"] == "succeeded"));
        let repaired: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(meta_dir.join("slimtoken-repair.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(repaired["after"]["passed"], true, "{repaired}");

        // The events in the bench's order, with the planned files.
        drop(emitter);
        let mut phases = Vec::new();
        let mut planned = None;
        let mut verified = false;
        let mut final_files = None;
        while let Ok(timed) = rx.try_recv() {
            match timed.ev {
                Ev::Phase { phase, .. } => phases.push(phase),
                Ev::Planned { files, .. } => planned = Some(files),
                Ev::Verified { after_failed, .. } => {
                    assert_eq!(after_failed, 0);
                    assert_eq!(phases.last(), Some(&Phase::Verify));
                    verified = true;
                }
                Ev::Final { ok, files, .. } => {
                    assert!(ok);
                    assert_eq!(phases.last(), Some(&Phase::Done));
                    final_files = Some(files);
                }
                _ => {}
            }
        }
        assert_eq!(phases, [Phase::Wave, Phase::Verify, Phase::Done]);
        assert!(verified);
        let planned = planned.expect("a Planned event");
        let expected: Vec<(String, bool)> = plan
            .tasks
            .iter()
            .filter(|task| !task.internal)
            .map(|task| (task.title.clone(), task.static_output.is_some()))
            .collect();
        let got: Vec<(String, bool)> = planned
            .iter()
            .map(|file| (file.path.clone(), file.rendered))
            .collect();
        assert_eq!(got, expected);
        let final_files = final_files.expect("a Final event");
        // Every planned file is written, the parts as the one style sheet.
        let delivered = joined.tasks.iter().filter(|task| !task.internal).count();
        assert!(delivered < expected.len());
        assert_eq!(
            final_files.len(),
            delivered,
            "every planned file is written"
        );
        assert!(!out_dir.join("app/styles").exists());
        for file in &final_files {
            assert!(out_dir.join(&file.path).is_file(), "{}", file.path);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn file_text_is_the_block_that_the_gate_reads() {
        let one = "```ts
export const a = 1;
```";
        assert_eq!(file_text("lib/a.ts", one).trim(), "export const a = 1;");
        let two = "```ts
export const a = 1;
```
```ts
export const a = 2;
```";
        assert_eq!(file_text("lib/a.ts", two).trim(), "export const a = 1;");
    }

    #[test]
    fn summary_prefers_test_totals_then_errors() {
        let vitest = "\u{1b}[32m ✓ tests/a.test.ts (12)\u{1b}[39m\n Test Files  2 passed (2)\n      Tests  31 passed (31)\n   Duration  1.2s\n";
        assert_eq!(summary_line(vitest), "Tests 31 passed (31)");
        let failed = " Tests  3 failed | 28 passed (31)\n Duration 2s\n";
        assert_eq!(summary_line(failed), "Tests 3 failed | 28 passed (31)");
        let build = "npm warn deprecated x\nType error: Property 'a' does not exist\nexit 1\n";
        assert_eq!(
            summary_line(build),
            "Type error: Property 'a' does not exist"
        );
        assert_eq!(summary_line("compiled\nok done\n"), "ok done");
    }

    /// `specwave`: the early tasks are the Types, Core and Data files and the
    /// internal tasks with no Consumer before them; a file keeps its early
    /// text only while its entry, the types and the files it names are
    /// planned the same.
    #[test]
    fn specwave_early_tasks_and_same_contract() {
        let staging = &slimtoken::profile::NEXTJS.staging;
        let task = |id: &str, title: &str, deps: &[&str], internal: bool| -> slimtoken::dag::Task {
            serde_json::from_value(serde_json::json!({
                "id": id, "title": title, "prompt": "p", "deps": deps, "internal": internal,
            }))
            .unwrap()
        };
        let plan = Plan {
            objective: "o".into(),
            tasks: vec![
                task("t", "lib/types.ts", &[], false),
                task("m", "lib/money.ts", &["t"], false),
                task("w", "wiring", &[], true),
                task("late", "summary", &["p"], true),
                task("p", "app/page.tsx", &["t", "m", "w", "c"], false),
                task("x", "tests/01_money.test.ts", &["m"], false),
                task("c", "app/styles/part1.css", &[], false),
                task("s", "lib/seed.ts", &[], false),
            ],
            cache_primes: Vec::new(),
        };
        let early = early_tasks(&plan, staging);
        let ids: Vec<&str> = early.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["t", "m", "w", "c", "s"]);
        // Nothing waits for the seed data.
        assert_eq!(free_tasks(&early, &plan), ["s"]);

        let file = |path: &str, export: &str, uses: &[&str]| slimtoken::architect::ManifestFile {
            path: path.into(),
            purpose: "p".into(),
            exports: vec![export.into()],
            uses: uses.iter().map(|used| used.to_string()).collect(),
        };
        let manifest = |files: Vec<slimtoken::architect::ManifestFile>| Manifest {
            files,
            test_files: Vec::new(),
            readme: false,
        };
        let base = || {
            vec![
                file("lib/types.ts", "export interface A {}", &[]),
                file("lib/money.ts", "export function f(): number", &[]),
                file(
                    "lib/due.ts",
                    "export function g(): number",
                    &["lib/money.ts:f"],
                ),
                file("app/page.tsx", "export default function Page()", &[]),
            ]
        };
        let first = manifest(base());
        // A page changed: the lib files are planned the same.
        let mut files = base();
        files[3].exports[0].push_str(" // more");
        let page = manifest(files);
        assert!(same_contract("lib/money.ts", &first, &page, staging));
        assert!(same_contract("lib/due.ts", &first, &page, staging));
        assert!(!same_contract("app/page.tsx", &first, &page, staging));
        // A lib file changed: it and the file that uses it are not.
        let mut files = base();
        files[1].exports.push("export function h(): number".into());
        let lib = manifest(files);
        assert!(!same_contract("lib/money.ts", &first, &lib, staging));
        assert!(!same_contract("lib/due.ts", &first, &lib, staging));
        // A new type: the types file is written again, a file that names
        // no changed type is kept.
        let mut files = base();
        files[0].exports.push("export interface B {}".into());
        let types = manifest(files);
        assert!(!same_contract("lib/types.ts", &first, &types, staging));
        assert!(same_contract("lib/money.ts", &first, &types, staging));
        // A changed type, and a type built on it: the files that name
        // either are not the same.
        let with = |a: &str| {
            let mut files = base();
            files[0].exports = vec![a.into(), "export interface Box { a: A }".into()];
            files[1].exports = vec!["export function f(box: Box): number".into()];
            manifest(files)
        };
        let (old, new) = (
            with("export interface A {}"),
            with("export interface A { n: number }"),
        );
        assert_eq!(
            changed_types(&old, &new, staging),
            Some((vec!["A".to_string(), "Box".to_string()], Vec::new()))
        );
        assert!(!same_contract("lib/money.ts", &old, &new, staging));
        assert!(same_contract("lib/due.ts", &old, &new, staging));
        // Only the comment of a type changed: the files that the new
        // clause names are not the same, the others that name the type are.
        let noted = |note: &str| {
            let mut files = base();
            files[0].exports = vec![format!("export interface A {{}}  // kept; {note}")];
            files[1].exports = vec!["export function f(a: A): number".into()];
            manifest(files)
        };
        let (old, new) = (noted("x"), noted("set by g (lib/due.ts)"));
        assert_eq!(
            changed_types(&old, &new, staging),
            Some((
                Vec::new(),
                vec!["x".to_string(), "set by g (lib/due.ts)".to_string()]
            ))
        );
        assert!(same_contract("lib/money.ts", &old, &new, staging));
        assert!(!same_contract("lib/due.ts", &old, &new, staging));
        assert_eq!(
            declared_type("export const DEFAULT: Settings  // x"),
            Some("DEFAULT")
        );
        assert!(has_word("a: Box[]", "Box") && !has_word("a: Boxes", "Box"));
    }
}
