use crate::client::{CompletionRequest, ModelClient};
use crate::control::validate_steering;
use crate::dag::{Plan, Task};
use crate::jsonfix;
use anyhow::{bail, Context, Result};

/// Uses one model call to create a machine-checked task DAG.
pub async fn generate_plan(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
) -> Result<Plan> {
    generate_plan_with_steering(client, objective, max_tasks, &[]).await
}

pub async fn generate_plan_with_steering(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
) -> Result<Plan> {
    validate_steering(steering)?;
    if objective.trim().is_empty() {
        bail!("objective cannot be empty");
    }
    if !(1..=256).contains(&max_tasks) {
        bail!("max_tasks must be between 1 and 256");
    }

    let system = "You decompose coding and agent objectives into the widest useful DAG of independent, small tasks. Return JSON only.".to_string();
    let steering_block = if steering.is_empty() {
        "None".to_string()
    } else {
        steering
            .iter()
            .enumerate()
            .map(|(index, directive)| format!("{}. {directive}", index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = format!(
        r#"Create an execution plan for this objective:

<objective>
{objective}
</objective>

User steering directives:
<steering>
{steering_block}
</steering>

Requirements:
- Apply the user's steering directives when choosing and wording tasks.
- Return between 1 and {max_tasks} tasks.
- Maximize useful parallelism: add an edge only for a real data dependency.
- Each task must have a narrow assignment and an explicit, useful output for downstream tasks.
- Do not make long linear chains when independent analysis, implementation, testing, or review can overlap.
- Include one final integration task that depends on every remaining leaf task.
- Workers currently produce text artifacts only; do not claim they can edit files or run tools.
- Reasoning models spend hidden tokens before answering: give every task a max_tokens budget of at least 1024 so the visible answer is never starved, and word assignments so the visible output stays concise.
- Use short unique IDs containing only letters, numbers, hyphens, or underscores.
- Omit dependencies on tasks that do not exist.

Return only this JSON shape, without Markdown or comments:
{{
  "objective": "repeat the objective",
  "tasks": [
    {{
      "id": "task-id",
      "title": "short title",
      "prompt": "specific assignment and expected output",
      "deps": [],
      "max_tokens": 1024,
      "temperature": 0.2
    }}
  ]
}}
"#
    );

    let response_tokens = (max_tasks as u32)
        .saturating_mul(256)
        .saturating_add(2_048)
        .min(16_384);
    // Try cheap reasoning first; escalate once if the plan does not parse.
    // Failed cheap plans are the exception, so the medium retry rarely fires.
    let attempt = |effort: &str| CompletionRequest {
        system: system.clone(),
        shared_context: None,
        user: user.clone(),
        max_tokens: response_tokens,
        temperature: 0.1,
        reasoning_effort: Some(effort.to_string()),
        prompt_cache_key: None,
        allow_empty: false,
        response_format: None,
    };
    let completion = client
        .complete(attempt("low"))
        .await
        .context("planner request failed")?;

    let json = match extract_json_object(&completion.text) {
        Ok(json) => json.to_string(),
        Err(_) => {
            // schema-off providers answer structured requests as free
            // text. Before regenerating the whole plan at medium effort, try
            // one small salvage call that repairs the failed output (same
            // parse + validate gate as fresh output).
            let salvaged = jsonfix::salvage(client, &completion.text, response_tokens, "low", None)
                .await
                .ok()
                .and_then(|fixed| extract_json_object(&fixed).ok().map(str::to_string));
            match salvaged {
                Some(json) => json,
                None => {
                    let retry = client
                        .complete(attempt("medium"))
                        .await
                        .context("planner retry at medium reasoning effort failed")?;
                    extract_json_object(&retry.text)
                        .context(
                            "planner returned no JSON object at low or medium reasoning effort",
                        )?
                        .to_string()
                }
            }
        }
    };
    // Deterministic repair (trailing commas, control characters, quote flip)
    // before giving up on malformed-but-present JSON.
    let mut plan: Plan = serde_json::from_str(&json)
        .or_else(|_| {
            jsonfix::repair(&json)
                .ok_or_else(|| anyhow::anyhow!("planner JSON is not repairable"))
                .and_then(|repaired| {
                    serde_json::from_str(&repaired).context("planner repair did not yield a plan")
                })
        })
        .context("planner returned invalid plan JSON")?;
    plan.objective = objective.to_string();
    plan.validate_with_limit(max_tasks)
        .context("planner returned an invalid task graph")?;
    Ok(plan)
}

fn extract_json_object(text: &str) -> Result<&str> {
    jsonfix::extract_object(text).context("planner response did not contain a JSON object")
}

/// A deterministic plan for testing the complete runner without a model server.
pub fn mock_plan(objective: &str) -> Plan {
    let make = |id: &str, title: &str, prompt: &str, deps: &[&str]| Task {
        id: id.into(),
        title: title.into(),
        prompt: prompt.into(),
        deps: deps.iter().map(|dep| (*dep).to_string()).collect(),
        max_tokens: 256,
        temperature: Some(0.0),
        require_closed_fences: false,
        validate_artifact: false,
        static_output: None,
        reasoning_effort: None,
        internal: false,
    };

    Plan {
        cache_primes: Vec::new(),
        objective: objective.into(),
        tasks: vec![
            make(
                "requirements",
                "Requirements",
                "Extract requirements and acceptance criteria.",
                &[],
            ),
            make(
                "architecture",
                "Architecture",
                "Propose a minimal architecture and identify extension points.",
                &[],
            ),
            make(
                "risks",
                "Risks",
                "Identify failure modes, security risks, and concurrency hazards.",
                &[],
            ),
            make(
                "test-strategy",
                "Test strategy",
                "Propose unit, integration, and load tests.",
                &[],
            ),
            make(
                "design",
                "Design synthesis",
                "Synthesize requirements, architecture, and risks into one design.",
                &["requirements", "architecture", "risks"],
            ),
            make(
                "implementation",
                "Implementation artifact",
                "Produce the implementation artifact required by the design.",
                &["design"],
            ),
            make(
                "tests",
                "Test artifact",
                "Produce tests from the test strategy and synthesized design.",
                &["test-strategy", "design"],
            ),
            make(
                "review",
                "Independent review",
                "Review the implementation for correctness and missing requirements.",
                &["implementation"],
            ),
            make(
                "verification",
                "Verification",
                "Check the implementation against the proposed tests.",
                &["implementation", "tests"],
            ),
            make(
                "integrate",
                "Final integration",
                "Resolve review and verification findings; return the final integrated answer.",
                &["review", "verification"],
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Completion;

    #[test]
    fn extracts_json_from_code_fences() {
        let input = "```json\n{\"objective\":\"x\",\"tasks\":[]}\n```";
        assert_eq!(
            extract_json_object(input).unwrap(),
            "{\"objective\":\"x\",\"tasks\":[]}"
        );
    }

    #[test]
    fn mock_plan_is_valid() {
        mock_plan("test objective").validate().unwrap();
    }

    /// distinguishes planner calls from salvage calls by the salvage
    /// system prompt; scripts one text per call shape.
    struct GateClient {
        plan: &'static str,
        salvage: &'static str,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl GateClient {
        fn new(plan: &'static str, salvage: &'static str) -> Self {
            Self {
                plan,
                salvage,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for GateClient {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = if request.system.contains("repair malformed JSON") {
                self.salvage
            } else {
                self.plan
            };
            Ok(Completion {
                text: text.to_string(),
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

    const PLAN_JSON: &str = r#"{"objective":"x","tasks":[{"id":"a","title":"t","prompt":"p","deps":[],"max_tokens":1024}]}"#;

    #[tokio::test]
    async fn salvage_repairs_unparseable_plan_without_medium_regeneration() {
        // Free-text failure on the low attempt; the salvage call repairs it.
        // Two calls total instead of the legacy low+medium regeneration pair.
        let client = GateClient::new("no json here", PLAN_JSON);
        let plan = generate_plan(&client, "objective", 8).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(client.calls(), 2);
    }

    #[tokio::test]
    async fn malformed_plan_json_repairs_deterministically_without_a_call() {
        // Trailing comma: extracted but unparseable; the deterministic repair
        // passes fix it on the spot (zero extra calls, zero tokens).
        let client = GateClient::new(
            r#"{"objective":"x","tasks":[{"id":"a","title":"t","prompt":"p","deps":[],"max_tokens":1024,}],}"#,
            "unused",
        );
        let plan = generate_plan(&client, "objective", 8).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(client.calls(), 1);
    }
}
