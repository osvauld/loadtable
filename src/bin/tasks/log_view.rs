//! Presentation only: retained logs and the socket API always keep the raw text.
pub fn readable_log(raw: &str) -> String {
    let mut output = String::new();
    for line in raw.lines() {
        let parsed = serde_json::from_str::<serde_json::Value>(line).ok();
        let text = match parsed.as_ref().and_then(|v| v["reason"].as_str()) {
            Some("compiler-message") => parsed
                .as_ref()
                .and_then(|v| v["message"]["rendered"].as_str()),
            Some("compiler-artifact") => {
                let value = parsed.as_ref().unwrap();
                if value["profile"]["test"] == true && value["executable"].is_string() {
                    output.push_str("Test harness ready: ");
                    output.push_str(value["target"]["name"].as_str().unwrap_or("unknown"));
                    output.push('\n');
                }
                continue;
            }
            Some("build-script-executed") => continue,
            Some("build-finished") => {
                match parsed.as_ref().unwrap()["success"].as_bool() {
                    Some(true) => output.push_str("Build succeeded\n"),
                    Some(false) => output.push_str("Build failed\n"),
                    None => {
                        output.push_str(line);
                        output.push('\n');
                    }
                }
                continue;
            }
            _ => None,
        };
        let text = text.unwrap_or(line);
        output.push_str(text);
        if !text.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_and_harnesses_replace_cargo_metadata() {
        let raw = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"dependency\"}}\n",
            "{\"reason\":\"build-script-executed\"}\n",
            "{\"reason\":\"compiler-message\",\"message\":{\"rendered\":\"warning: example\\n\"}}\n",
            "{\"reason\":\"compiler-artifact\",\"profile\":{\"test\":true},\"executable\":\"/test\",\"target\":{\"name\":\"contracts\"}}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n",
            "test result: ok. 7 passed\n"
        );
        assert_eq!(
            readable_log(raw),
            "warning: example\nTest harness ready: contracts\nBuild succeeded\ntest result: ok. 7 passed\n"
        );
    }

    #[test]
    fn unknown_json_missing_diagnostics_and_partial_tail_are_preserved() {
        let raw = "partial JSON}\n{\"reason\":\"future-event\"}\n{\"reason\":\"compiler-message\"}\nworker error: failed";
        assert_eq!(readable_log(raw), format!("{raw}\n"));
        assert_eq!(readable_log(""), "");
    }

    #[test]
    fn failed_build_remains_visible() {
        assert_eq!(
            readable_log("{\"reason\":\"build-finished\",\"success\":false}"),
            "Build failed\n"
        );
    }
}
