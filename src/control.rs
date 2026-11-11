use crate::dag::Plan;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

const MAX_STEERING_DIRECTIVES: usize = 32;
const MAX_STEERING_CHARS: usize = 8_000;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskProgressStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
}

impl std::fmt::Display for TaskProgressStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskProgress {
    pub id: String,
    pub title: String,
    pub status: TaskProgressStatus,
    pub attempts: u32,
    pub updated_at_ms: u64,
}

#[derive(Debug)]
struct ControlInner {
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
    steering: RwLock<Vec<String>>,
    tasks: RwLock<BTreeMap<String, TaskProgress>>,
}

/// Shared control plane used by the scheduler, interactive CLI, and REST API.
#[derive(Debug, Clone)]
pub struct RunControl {
    inner: Arc<ControlInner>,
}

impl Default for RunControl {
    fn default() -> Self {
        Self::with_steering(Vec::new()).expect("empty steering is valid")
    }
}

impl RunControl {
    pub fn with_steering(steering: Vec<String>) -> Result<Self> {
        validate_steering(&steering)?;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        Ok(Self {
            inner: Arc::new(ControlInner {
                cancel_tx,
                cancel_rx,
                steering: RwLock::new(steering),
                tasks: RwLock::new(BTreeMap::new()),
            }),
        })
    }

    pub fn steer(&self, instruction: impl Into<String>) -> Result<()> {
        let instruction = instruction.into();
        validate_steering(std::slice::from_ref(&instruction))?;
        let mut steering = self.inner.steering.write().expect("steering lock");
        if steering.len() >= MAX_STEERING_DIRECTIVES {
            bail!("a run accepts at most {MAX_STEERING_DIRECTIVES} steering directives");
        }
        steering.push(instruction);
        Ok(())
    }

    pub fn steering(&self) -> Vec<String> {
        self.inner.steering.read().expect("steering lock").clone()
    }

    pub fn cancel(&self) {
        self.inner.cancel_tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.inner.cancel_rx.borrow()
    }

    /// Completes promptly when cancellation is requested, including while a
    /// scheduler is waiting for the next model result.
    pub async fn cancelled(&self) {
        let mut receiver = self.inner.cancel_tx.subscribe();
        if *receiver.borrow() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow() {
                return;
            }
        }
    }

    pub fn register_plan(&self, plan: &Plan) {
        let mut tasks = self.inner.tasks.write().expect("task progress lock");
        tasks.clear();
        for task in &plan.tasks {
            tasks.insert(
                task.id.clone(),
                TaskProgress {
                    id: task.id.clone(),
                    title: task.title.clone(),
                    status: TaskProgressStatus::Pending,
                    attempts: 0,
                    updated_at_ms: unix_time_ms(),
                },
            );
        }
    }

    pub fn task_started(&self, id: &str) {
        self.update_task(id, TaskProgressStatus::Running, 0);
    }

    pub fn task_finished(&self, id: &str, status: TaskProgressStatus, attempts: u32) {
        self.update_task(id, status, attempts);
    }

    fn update_task(&self, id: &str, status: TaskProgressStatus, attempts: u32) {
        if let Some(task) = self
            .inner
            .tasks
            .write()
            .expect("task progress lock")
            .get_mut(id)
        {
            task.status = status;
            task.attempts = attempts;
            task.updated_at_ms = unix_time_ms();
        }
    }

    pub fn tasks(&self) -> BTreeMap<String, TaskProgress> {
        self.inner.tasks.read().expect("task progress lock").clone()
    }
}

pub fn validate_steering(steering: &[String]) -> Result<()> {
    if steering.len() > MAX_STEERING_DIRECTIVES {
        bail!("a run accepts at most {MAX_STEERING_DIRECTIVES} steering directives");
    }
    for directive in steering {
        if directive.trim().is_empty() {
            bail!("steering directives cannot be empty");
        }
        if directive.chars().count() > MAX_STEERING_CHARS {
            bail!("one steering directive cannot exceed {MAX_STEERING_CHARS} characters");
        }
    }
    Ok(())
}

pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
