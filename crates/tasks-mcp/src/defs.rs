//! Tool definitions: names, descriptions and closed input schemas (R-195).

use serde_json::{Value, json};

/// The tool names, in the order `tools/list` returns them.
pub const TOOL_NAMES: [&str; 13] = [
    "submit",
    "submit_steps",
    "run_suite",
    "list",
    "get",
    "logs",
    "wait",
    "cancel",
    "prioritize",
    "status",
    "estimate",
    "audit",
    "gc",
];

fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn id_prop(what: &str) -> Value {
    json!({"type": "integer", "minimum": 1, "description": what})
}

fn who_prop() -> Value {
    json!({"type": "string", "description": "Your unique agent name, recorded as the job owner (letters, digits, '.', '_', '-'). Default: $LOADTABLE_WHO, then $USER of the MCP server."})
}

fn cwd_prop() -> Value {
    json!({"type": "string", "description": "Absolute path of the checkout the work runs in. Default: the MCP server's working directory."})
}

fn placement_props() -> (Value, Value) {
    (
        json!({"type": "string", "enum": ["here", "pin", "anywhere"],
            "description": "Where the work may run. here (default): this machine, the daemon may reroute a waiting job. pin: this machine, never rerouted. anywhere: any capable machine."}),
        json!({"type": "string", "description": "Run on this named peer machine instead (do not combine with placement)."}),
    )
}

fn priority_props() -> (Value, Value) {
    (
        json!({"type": "string", "enum": ["blocking", "normal", "background"],
            "description": "Queue class. normal (default). background: warm-ups, long suites and sweeps, which wait behind everything else and rise to normal after 20 minutes. blocking: you are waiting on the result right now; needs reason and is audited. The class orders the queue only; it never preempts a running job and never makes a job 'short' (the short lane comes from measured history alone)."}),
        json!({"type": "string", "description": "Why the job is blocking (required with priority blocking; kept in the audit log)."}),
    )
}

/// One tool: name, description and input schema.
pub struct ToolDef {
    /// The tool name.
    pub name: &'static str,
    /// What it does, for the model.
    pub description: &'static str,
    /// The input schema: a closed object.
    pub input_schema: Value,
}

impl ToolDef {
    /// The `tools/list` entry.
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "description": self.description, "inputSchema": self.input_schema})
    }
}

/// Every tool, in [`TOOL_NAMES`] order.
pub fn tool_defs() -> Vec<ToolDef> {
    let (placement, peer) = placement_props();
    let (priority, reason) = priority_props();
    let step = object(
        json!({
            "name": {"type": "string", "description": "Unique step name (letters, digits, '.', '_', '-')."},
            "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Program and arguments."},
            "after": {"type": "array", "items": {"type": "string"}, "description": "Names of steps that must pass first."},
            "kind": {"type": "string", "enum": ["compile", "test", "run", "package", "device", "probe"], "description": "Admission class (default compile)."},
            "fetch": {"type": "array", "items": {"type": "string"}, "maxItems": 256, "description": "Output files to bring back to this checkout after the job ran on a peer, as paths relative to the checkout root (e.g. android/app/build/outputs/apk/debug/app-debug.apk). Absolute paths and .. are refused. The result of each is reported by wait/get; a job that ran here already has its files."}
        }),
        &["name", "argv"],
    );
    vec![
        ToolDef {
            name: "submit",
            description: "Queue one command through the shared build/test queue and return its job id at once (use wait or logs afterwards). Same rules as `loadtable run`: a broad `cargo test` (name filter without --lib/--test) is refused unless broad_test is true; a `bash -c` script of several commands is refused unless allow_script is true (use submit_steps or run_suite); a device serial the machine policy denies is refused.",
            input_schema: object(
                json!({
                    "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Program and arguments, e.g. [\"cargo\",\"test\",\"-p\",\"tasks-mcp\"]."},
                    "cwd": cwd_prop(),
                    "who": who_prop(),
                    "placement": placement,
                    "peer": peer,
                    "device": {"type": "string", "description": "adb serial of an attached test device; the job holds that device's slot and stays on this machine."},
                    "os": {"type": "string", "enum": ["linux", "macos"], "description": "Only machines with this OS may run it (e.g. a build whose binary you will run here, or that needs Linux libraries). Without it, a placement \"anywhere\" job takes the repo run policy's [placement] os."},
                    "timeout_s": {"type": "integer", "minimum": 1, "description": "Stop the job after this many seconds (1 s to 7 days; the job ends timed out)."},
                    "broad_test": {"type": "boolean", "description": "Deliberately build every test target of a filtered `cargo test`. Prefer naming the target."},
                    "allow_script": {"type": "boolean", "description": "Run a `sh -c`/`bash -c` script of several commands as one job. Without it such a script is refused: it gets no timing history or estimate. Prefer submit_steps, run_suite, or a script file in the repo."},
                    "after": {"type": "array", "items": {"type": "integer", "minimum": 1}, "description": "Job ids that must pass before this job may start."},
                    "fetch": {"type": "array", "items": {"type": "string"}, "maxItems": 256, "description": "Output files to bring back to this checkout after the job ran on a peer, as paths relative to the checkout root (e.g. android/app/build/outputs/apk/debug/app-debug.apk). Absolute paths and .. are refused. The result of each is reported by wait/get; a job that ran here already has its files."},
                    "interactive": {"type": "boolean", "description": "The job opens a window or waits on a person (a GUI, a REPL): it runs as long as it is used, so it gets no estimate or \"longer than recent runs\" warning and its runs teach no timings."},
                    "priority": priority.clone(),
                    "reason": reason.clone()
                }),
                &["argv"],
            ),
        },
        ToolDef {
            name: "submit_steps",
            description: "Queue a plan of named, dependent steps at once. Each step is checked with the same rules as submit; nothing is sent to the daemon unless all pass. Returns the plan id and one job id per step.",
            input_schema: object(
                json!({
                    "steps": {"type": "array", "items": step, "minItems": 1, "maxItems": 256},
                    "cwd": cwd_prop(),
                    "who": who_prop(),
                    "placement": placement,
                    "peer": peer,
                    "broad_test": {"type": "boolean", "description": "Allow broad `cargo test` steps."},
                    "priority": priority.clone(),
                    "reason": reason.clone()
                }),
                &["steps"],
            ),
        },
        ToolDef {
            name: "run_suite",
            description: "Queue a suite of the repo's run policy (.loadtable/run-policy.toml) as a plan. Returns the plan id and one job id per step.",
            input_schema: object(
                json!({
                    "name": {"type": "string", "description": "The suite name."},
                    "cwd": cwd_prop(),
                    "who": who_prop(),
                    "placement": placement,
                    "peer": peer,
                    "quick": {"type": "boolean", "description": "Drop steps marked skip_in_quick."},
                    "broad_test": {"type": "boolean", "description": "Allow broad `cargo test` steps."},
                    "priority": priority.clone(),
                    "reason": reason.clone()
                }),
                &["name"],
            ),
        },
        ToolDef {
            name: "list",
            description: "List jobs in the shared ledger, oldest first: all unfinished jobs plus the newest finished ones.",
            input_schema: object(
                json!({
                    "who": {"type": "string", "description": "Only jobs of this owner."},
                    "active": {"type": "boolean", "description": "Only unfinished jobs."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Newest finished jobs to include (default 20)."}
                }),
                &[],
            ),
        },
        ToolDef {
            name: "get",
            description: "One job by id: phase, exit code, owner, cwd, command, timings, where it runs, why it waits.",
            input_schema: object(json!({"id": id_prop("The job id.")}), &["id"]),
        },
        ToolDef {
            name: "logs",
            description: "Read a job's retained log, bounded. Default: the last 100 lines. With grep: matching lines with line numbers. With offset: bytes from that offset on, to follow a running job; the reply names the next offset.",
            input_schema: object(
                json!({
                    "id": id_prop("The job id."),
                    "tail": {"type": "integer", "minimum": 1, "maximum": 5000, "description": "Last N lines (default 100)."},
                    "grep": {"type": "string", "description": "Show lines matching this pattern (regex subset; see fixed)."},
                    "fixed": {"type": "boolean", "description": "Read grep as a plain substring."},
                    "context": {"type": "integer", "minimum": 0, "maximum": 100, "description": "Lines of context around each grep match."},
                    "max_matches": {"type": "integer", "minimum": 1, "description": "Most grep matches returned."},
                    "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to read from (use the next_offset of the previous reply)."},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 262_144, "description": "Most bytes returned with offset (default 32768)."}
                }),
                &["id"],
            ),
        },
        ToolDef {
            name: "wait",
            description: "Wait until a job is terminal or timeout_s passes, then report its state and the log tail. Cancelling this request never touches the job; call wait again to keep waiting. Keep timeout_s below your client's tool timeout.",
            input_schema: object(
                json!({
                    "id": id_prop("The job id."),
                    "timeout_s": {"type": "integer", "minimum": 0, "description": "Seconds to wait (default 120, at most 3600). 0 checks once."},
                    "tail_lines": {"type": "integer", "minimum": 0, "maximum": 500, "description": "Log lines to include once the job is over (default 20)."}
                }),
                &["id"],
            ),
        },
        ToolDef {
            name: "cancel",
            description: "Cancel one of YOUR jobs: a queued job at once, a running job by stopping its process group. Refused unless who equals the job's owner.",
            input_schema: object(
                json!({
                    "id": id_prop("The job id."),
                    "who": {"type": "string", "description": "Your agent name; must equal the job's owner."},
                    "reason": {"type": "string", "description": "Why, for the record."}
                }),
                &["id", "who"],
            ),
        },
        ToolDef {
            name: "prioritize",
            description: "Change the queue class of one of YOUR jobs that has not started yet (raise a job someone now waits on to blocking, or lower one to background). Refused unless who equals the job's owner, and refused for a running or finished job: priority orders the queue and never preempts. blocking needs a reason; the change is audited.",
            input_schema: object(
                json!({
                    "id": id_prop("The job id."),
                    "who": {"type": "string", "description": "Your agent name; must equal the job's owner."},
                    "priority": priority.clone(),
                    "reason": reason.clone()
                }),
                &["id", "who", "priority"],
            ),
        },
        ToolDef {
            name: "status",
            description: "The machine now: slots free and busy, running and waiting jobs, memory/cpu/io pressure, disk, what blocks a new compile, alerts and peer links.",
            input_schema: object(json!({}), &[]),
        },
        ToolDef {
            name: "estimate",
            description: "Estimate how long a command takes on this machine from past runs.",
            input_schema: object(
                json!({"argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "The command."}}),
                &["argv"],
            ),
        },
        ToolDef {
            name: "audit",
            description: "Read the machine's audit log, newest first: who did what and when (pairing and grants, refused connections, peer link changes, job submit/forward/receive/start/end/cancel/timeout, drain and resume, submit-rule refusals). Read-only. Each row has an actor machine, an owner, an action, a job or peer and details; no secret is ever stored in it.",
            input_schema: object(
                json!({
                    "since": {"type": "string", "description": "Only rows since: a duration back from now such as 30m, 2h or 3d, or unix seconds."},
                    "peer": {"type": "string", "description": "Only rows naming this paired machine (its name or DID) as the peer or the actor."},
                    "job": {"type": "integer", "minimum": 1, "description": "Only rows about this job."},
                    "action": {"type": "string", "description": "Only this action (job.cancelled) or a family (job, pair, grant, conn, queue)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Most rows (default 50)."}
                }),
                &[],
            ),
        },
        ToolDef {
            name: "gc",
            description: "What each checkout's build output takes on this machine and its peers, and freeing it: a checkout is in_use (a job queued or running), active, idle (no job in idle_days), merged (its branch is in dev/main/master, nothing tracked changed), gone (its working tree is deleted) or old (an older layout's output). Without delete it only reports. delete frees only build output and peers' synced copies, never a working tree and never an in_use checkout; the next build there starts cold.",
            input_schema: object(
                json!({
                    "delete": {"type": "array", "items": {"type": "string", "enum": ["merged", "gone", "old", "idle", "active"]}, "description": "Free the checkouts in these states. Usually [\"merged\", \"gone\", \"old\"]."},
                    "idle_days": {"type": "integer", "minimum": 1, "description": "Days without a job after which a checkout is idle (default 14)."}
                }),
                &[],
            ),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // R-195
    #[test]
    fn r195_tool_schemas_are_closed_objects_whose_required_keys_exist() {
        fn check(schema: &Value, path: &str) {
            let Some(obj) = schema.as_object() else {
                return;
            };
            if obj.get("type").and_then(Value::as_str) == Some("object") {
                assert_eq!(
                    obj.get("additionalProperties"),
                    Some(&Value::Bool(false)),
                    "{path} is not closed"
                );
                let props = obj
                    .get("properties")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| panic!("{path} has no properties"));
                for r in obj
                    .get("required")
                    .and_then(Value::as_array)
                    .unwrap_or_else(|| panic!("{path} has no required list"))
                {
                    let key = r.as_str().unwrap_or_default();
                    assert!(props.contains_key(key), "{path}: required {key} is missing");
                }
                for (k, v) in props {
                    check(v, &format!("{path}.{k}"));
                }
            }
            if let Some(items) = obj.get("items") {
                check(items, &format!("{path}[]"));
            }
        }
        for d in tool_defs() {
            assert_eq!(d.input_schema["type"], "object", "{}", d.name);
            check(&d.input_schema, d.name);
        }
    }

    #[test]
    fn names_match_the_constant_and_are_unique() {
        let names: Vec<&str> = tool_defs().iter().map(|d| d.name).collect();
        assert_eq!(names, TOOL_NAMES);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }
}
