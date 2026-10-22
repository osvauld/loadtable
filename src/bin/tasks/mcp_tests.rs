//! In-process tests of the MCP server: protocol handling against a captured
//! output stream, argument validation and the `tasks run` submission policy.
//! The subprocess tests against a fake daemon are in tests/mcp.rs.
use super::*;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);
impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Buffer {
    fn messages(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every output line is JSON"))
            .collect()
    }
}

fn server() -> (Arc<Server>, Buffer) {
    let buffer = Buffer::default();
    let server = Arc::new(Server {
        daemon: Daemon {
            socket: "/nonexistent/tasks-mcp-unit/taskd.sock".into(),
            autostart: false,
        },
        out: Out::new(Box::new(buffer.clone())),
        in_flight: Mutex::default(),
        shutdown: Arc::default(),
    });
    (server, buffer)
}

/// Feeds lines, waits for started tool threads, returns everything written.
fn exchange(lines: &[&str]) -> Vec<Value> {
    let (server, buffer) = server();
    for line in lines {
        if let Some(thread) = server.handle_line(line.as_bytes()) {
            thread.join().unwrap();
        }
    }
    buffer.messages()
}

fn ctx() -> Ctx {
    Ctx {
        cancel: Arc::default(),
        shutdown: Arc::default(),
        out: Out::new(Box::new(Buffer::default())),
        progress: None,
        sent: AtomicU64::new(0),
    }
}

#[test]
fn initialize_negotiates_the_protocol_version() {
    let out = exchange(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#,
    ]);
    assert_eq!(out.len(), 3, "a notification gets no reply: {out:?}");
    assert_eq!(out[0]["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(out[1]["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(out[1]["result"]["serverInfo"]["name"], "tasks");
    assert!(out[1]["result"]["capabilities"]["tools"].is_object());
    assert_eq!(out[2], json!({"jsonrpc":"2.0","id":"p","result":{}}));
}

#[test]
fn malformed_messages_get_json_rpc_errors() {
    let out = exchange(&[
        "{not json",
        "[]",
        "42",
        r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":2}"#,
        r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"job","arguments":[1]}}"#,
        r#"{"jsonrpc":"2.0","id":7,"method":"ping","params":[]}"#,
        "",
        r#"{"jsonrpc":"2.0","method":"notifications/unknown"}"#,
    ]);
    let codes: Vec<(Value, i64)> = out
        .iter()
        .map(|m| (m["id"].clone(), m["error"]["code"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        codes,
        vec![
            (Value::Null, PARSE_ERROR),
            (Value::Null, INVALID_REQUEST),
            (Value::Null, INVALID_REQUEST),
            (json!(1), INVALID_REQUEST),
            (json!(2), INVALID_REQUEST),
            (Value::Null, INVALID_REQUEST),
            (json!(3), METHOD_NOT_FOUND),
            (json!(4), INVALID_PARAMS),
            (json!(5), INVALID_PARAMS),
            (json!(6), INVALID_PARAMS),
            (json!(7), INVALID_PARAMS),
        ]
    );
    assert!(out.iter().all(|m| m["jsonrpc"] == "2.0"));
}

#[test]
fn tool_schemas_are_closed_objects_whose_required_keys_exist() {
    let tools = tools();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOL_NAMES);
    for tool in &tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{tool}");
        assert_eq!(schema["additionalProperties"], false, "{tool}");
        assert!(tool["description"].as_str().unwrap().len() > 40, "{tool}");
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("placement"));
        for key in schema["required"].as_array().into_iter().flatten() {
            assert!(properties.contains_key(key.as_str().unwrap()), "{tool}");
        }
    }
    let submit = &tools[4];
    assert_eq!(
        submit["inputSchema"]["required"],
        json!(["who", "cwd", "argv"])
    );
    let description = submit["description"].as_str().unwrap();
    assert!(description.contains("ZD22257P6G") && description.contains("-j"));
}

#[test]
fn arguments_are_checked_before_any_daemon_request() {
    let daemon = Daemon {
        socket: "/nonexistent/taskd.sock".into(),
        autostart: false,
    };
    let call =
        |name: &str, args: Value| call_tool(&daemon, name, args.as_object().unwrap(), &ctx());
    for (name, args, needle) in [
        ("job", json!({}), "missing required argument \"id\""),
        ("job", json!({"id": 0}), "id must be an integer"),
        ("job", json!({"id": "7"}), "id must be an integer"),
        (
            "job",
            json!({"id": 7, "idd": 1}),
            "unknown argument \"idd\"",
        ),
        ("job", json!({"id": 7, "placement": "worker"}), "placement"),
        ("list_jobs", json!({"phases": ["done"]}), "unknown phase"),
        ("list_jobs", json!({"limit": 0}), "limit must be"),
        (
            "submit",
            json!({"who": "a", "cwd": "/", "argv": "ls"}),
            "argv must be",
        ),
        (
            "cancel",
            json!({"id": 1}),
            "missing required argument \"who\"",
        ),
        (
            "submit_steps",
            json!({"who": "a", "cwd": "/", "steps": [{"name": "a", "argv": ["x"], "after": ["ghost"]}]}),
            "not a step",
        ),
        (
            "submit_steps",
            json!({"who": "a", "cwd": "/", "steps": []}),
            "at least one",
        ),
        (
            "submit",
            json!({"who": "a", "cwd": "/", "argv": ["ls"], "after": ["x"]}),
            "after must be",
        ),
    ] {
        let result = call(name, args);
        assert_eq!(result["isError"], true, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(needle), "{name}: {text}");
    }
    // Valid arguments reach the (absent) daemon and fail cleanly.
    let result = call("job", json!({"id": 7}));
    assert_eq!(result["isError"], true);
    assert!(
        result["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("tasks daemon"),
        "{result}"
    );
}

#[test]
fn submission_policy_is_the_tasks_run_policy() {
    let cwd = std::env::temp_dir();
    let cwd = cwd.to_str().unwrap();
    let spec = run_spec(
        "synthetic-mcp",
        cwd,
        vec!["cargo".into(), "test".into(), "-j".into(), "2".into()],
        "compile",
        None,
    )
    .unwrap();
    assert_eq!(spec.suite, tinline_task_manager::local_command::SUITE);
    assert_eq!(spec.revision, "live-workspace");
    assert_eq!(
        spec.source,
        std::fs::canonicalize(cwd).unwrap().to_str().unwrap()
    );
    let recipe: Recipe = serde_json::from_str(&spec.configuration).unwrap();
    assert_eq!(recipe.resource, Resource::Compile);
    let device = run_spec(
        "w",
        cwd,
        vec!["adb".into()],
        "device",
        Some("10.0.0.2:5555"),
    )
    .unwrap();
    let recipe: Recipe = serde_json::from_str(&device.configuration).unwrap();
    assert_eq!(recipe.resource, Resource::Device("10.0.0.2_5555".into()));
    let refused = |who: &str, cwd: &str, argv: &[&str], class: &str, serial: Option<&str>| {
        run_spec(
            who,
            cwd,
            argv.iter().map(|s| s.to_string()).collect(),
            class,
            serial,
        )
        .unwrap_err()
        .to_string()
    };
    assert!(refused("w", cwd, &["adb"], "device", Some("ZD22257P6G")).contains("denylisted"));
    assert!(
        refused("w", cwd, &["adb", "-s", "zd22257p6g", "shell"], "run", None)
            .contains("denylisted")
    );
    assert!(
        refused("w", cwd, &["cargo", "build", "-j", "64"], "compile", None).contains("Cargo jobs")
    );
    assert!(refused("w", cwd, &["python3", "buildlock.py"], "run", None).contains("nest"));
    assert!(refused(" ", cwd, &["true"], "run", None).contains("bounded owner"));
    assert!(refused("w", "relative/dir", &["true"], "run", None).contains("absolute"));
    assert!(refused("w", cwd, &["true"], "device", None).contains("device_serial"));
    assert!(refused("w", cwd, &["true"], "run", Some("emulator-5554")).contains("class device"));
    assert!(refused("w", cwd, &["true"], "offload", None).contains("class must be"));
    assert!(refused("w", cwd, &[], "run", None).contains("argv"));
}

#[test]
fn cancelled_requests_get_no_response() {
    let (server, buffer) = server();
    let flag = Arc::new(AtomicBool::new(false));
    server
        .in_flight
        .lock()
        .unwrap()
        .insert("9".into(), flag.clone());
    assert!(
        server
            .handle_line(
                br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#
            )
            .is_none()
    );
    assert!(flag.load(Ordering::SeqCst));
    assert!(buffer.messages().is_empty());
}
