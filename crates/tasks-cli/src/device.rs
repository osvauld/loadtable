//! Android target aliases, canonicalised on the client (R-213).

/// The canonical ABI list for a target the user typed. `phone` and `emulator` are input
/// aliases only; the daemon accepts the canonical strings and nothing else.
pub fn canonical_android_target(input: &str) -> Result<&'static str, String> {
    match input.trim() {
        "phone" | "arm64-v8a" => Ok("arm64-v8a"),
        "emulator" | "x86_64" => Ok("x86_64"),
        "x86_64,arm64-v8a" | "arm64-v8a,x86_64" => Ok("x86_64,arm64-v8a"),
        other => Err(format!(
            "unknown Android target {other:?}; use phone (arm64-v8a), emulator (x86_64) or x86_64,arm64-v8a"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // R-213
    #[test]
    fn r213_android_target_aliases_canonicalize_before_submission() {
        assert_eq!(canonical_android_target("phone"), Ok("arm64-v8a"));
        assert_eq!(canonical_android_target("emulator"), Ok("x86_64"));
        assert_eq!(canonical_android_target("arm64-v8a"), Ok("arm64-v8a"));
        assert_eq!(
            canonical_android_target("arm64-v8a,x86_64"),
            Ok("x86_64,arm64-v8a")
        );
        let e = canonical_android_target("tablet").unwrap_err();
        assert!(e.contains("phone (arm64-v8a)") && e.contains("\"tablet\""));
    }
}
