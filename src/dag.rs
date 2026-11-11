use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

fn default_max_tokens() -> u32 {
    1_024
}

/// A single unit of model work. Dependencies refer to other task IDs.
///
/// All flags default to off, so `..Default::default()` yields a plain task
/// with a 1024-token output budget (matching the serde default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// When true, a successful completion whose output has an unbalanced
    /// code-fence count (a fence line opened but never closed) is treated
    /// as a failed attempt and retried. Servers occasionally end a stream
    /// mid-content while still reporting a clean finish, which would
    /// otherwise hand downstream tasks a silently truncated artifact.
    /// Intended for tasks whose contract is fenced artifacts (the harness's
    /// integration sink sets this).
    #[serde(default)]
    pub require_closed_fences: bool,
    /// When true, the scheduler pipes the successful output through the
    /// run's artifact validator (see `Validator`); a rejection converts the
    /// success into a failure carrying the validator's message, which the
    /// repair wave then hands back to the model as context. Set by planners
    /// whose artifacts have a cheap mechanical check (architect: Python
    /// modules/tests get syntax validation; prose tasks like README stay
    /// false).
    #[serde(default)]
    pub validate_artifact: bool,
    /// Precomputed artifact: the scheduler records it as the task's output
    /// with zero model calls. Planners use it for deliverables derivable
    /// deterministically from data they already hold (the architect renders
    /// the README from its manifest), so the wave spends no tokens or slot
    /// on them while assembly and reports stay uniform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_output: Option<String>,
    /// Per-task reasoning effort, overriding the run's `reasoning_effort`.
    /// Lets a planner spend reasoning only where a task needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Working data for other tasks (e.g. the in-wave wiring plan), not part
    /// of the deliverable: final-output assembly skips it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal: bool,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            prompt: String::new(),
            deps: Vec::new(),
            max_tokens: default_max_tokens(),
            temperature: None,
            require_closed_fences: false,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
        }
    }
}

/// The planner's output: an objective plus a validated directed acyclic graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub objective: String,
    pub tasks: Vec<Task>,
    /// Prompts whose ONLY purpose is to be cheaply completed (max_tokens ~1)
    /// so the model server stores their byte-prefix before the wave launches.
    /// Every worker whose prompt starts with a prime then hits the prefix
    /// cache instead of racing to be first. Set by planners that lay tasks
    /// out cache-aware (contracts-first); empty for everything else.
    #[serde(default)]
    pub cache_primes: Vec<String>,
}

impl Plan {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_limit(256)
    }

    pub fn validate_with_limit(&self, max_tasks: usize) -> Result<()> {
        if self.objective.trim().is_empty() {
            bail!("plan objective cannot be empty");
        }
        if self.tasks.is_empty() {
            bail!("plan must contain at least one task");
        }
        if self.tasks.len() > max_tasks {
            bail!(
                "plan contains {} tasks, exceeding the limit of {max_tasks}",
                self.tasks.len()
            );
        }

        let mut ids = HashSet::with_capacity(self.tasks.len());
        for task in &self.tasks {
            if task.id.trim().is_empty() {
                bail!("task IDs cannot be empty");
            }
            if task.title.trim().is_empty() {
                bail!("task '{}' has an empty title", task.id);
            }
            if task.prompt.trim().is_empty() {
                bail!("task '{}' has an empty prompt", task.id);
            }
            if task.max_tokens == 0 {
                bail!("task '{}' has max_tokens set to zero", task.id);
            }
            if !ids.insert(task.id.as_str()) {
                bail!("duplicate task ID '{}'", task.id);
            }
        }

        let mut indegree: HashMap<&str, usize> = ids.iter().map(|id| (*id, 0usize)).collect();
        let mut dependents: HashMap<&str, Vec<&str>> =
            ids.iter().map(|id| (*id, Vec::new())).collect();

        for task in &self.tasks {
            let mut seen_deps = HashSet::new();
            for dep in &task.deps {
                if !ids.contains(dep.as_str()) {
                    bail!("task '{}' depends on unknown task '{}'", task.id, dep);
                }
                if dep == &task.id {
                    bail!("task '{}' depends on itself", task.id);
                }
                if !seen_deps.insert(dep.as_str()) {
                    bail!(
                        "task '{}' lists dependency '{}' more than once",
                        task.id,
                        dep
                    );
                }
                *indegree
                    .get_mut(task.id.as_str())
                    .context("missing indegree entry")? += 1;
                dependents
                    .get_mut(dep.as_str())
                    .context("missing dependents entry")?
                    .push(task.id.as_str());
            }
        }

        // Kahn's algorithm detects cycles before the scheduler starts.
        let mut ready: VecDeque<&str> = indegree
            .iter()
            .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
            .collect();
        let mut visited = 0usize;

        while let Some(id) = ready.pop_front() {
            visited += 1;
            for &child in dependents.get(id).context("missing graph node")? {
                let degree = indegree.get_mut(child).context("missing graph edge")?;
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(child);
                }
            }
        }

        if visited != self.tasks.len() {
            bail!("plan contains a dependency cycle");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: format!("Task {id}"),
            prompt: format!("Do {id}"),
            deps: deps.iter().map(|dep| (*dep).to_string()).collect(),
            max_tokens: 128,
            temperature: None,
            require_closed_fences: false,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
        }
    }

    #[test]
    fn accepts_a_valid_dag() {
        let plan = Plan {
            cache_primes: Vec::new(),
            objective: "test".into(),
            tasks: vec![task("a", &[]), task("b", &[]), task("c", &["a", "b"])],
        };
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn rejects_cycles() {
        let plan = Plan {
            cache_primes: Vec::new(),
            objective: "test".into(),
            tasks: vec![task("a", &["b"]), task("b", &["a"])],
        };
        assert!(plan.validate().unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn rejects_unknown_dependencies() {
        let plan = Plan {
            cache_primes: Vec::new(),
            objective: "test".into(),
            tasks: vec![task("a", &["missing"])],
        };
        assert!(plan
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unknown task"));
    }
}
