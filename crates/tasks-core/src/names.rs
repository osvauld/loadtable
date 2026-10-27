//! The product's names, and the older `tasks2` ones still honoured after the rename to LoadTable
//! (R-804).

/// The directory name under the XDG state and config homes and under the cache root.
pub const DIR_NAME: &str = "loadtable";
/// The directory name before the rename; its contents are moved to [`DIR_NAME`] on start.
pub const OLD_DIR_NAME: &str = "tasks2";

/// Prefix of the environment settings.
const ENV_PREFIX: &str = "LOADTABLE_";
/// Prefix of the same settings before the rename.
const OLD_ENV_PREFIX: &str = "TASKS_";

/// Reads `key` (a `LOADTABLE_` setting) from `get`, then its older `TASKS_` name, so an
/// environment written for `tasks2` keeps working.
pub fn env_or_old(get: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key).or_else(|| {
        key.strip_prefix(ENV_PREFIX)
            .and_then(|rest| get(&format!("{OLD_ENV_PREFIX}{rest}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r804_a_loadtable_setting_falls_back_to_its_tasks_name() {
        let env = |k: &str| match k {
            "TASKS_WHO" => Some("old".to_string()),
            "LOADTABLE_MACHINE" => Some("new".to_string()),
            "TASKS_MACHINE" => Some("ignored".to_string()),
            _ => None,
        };
        assert_eq!(env_or_old(&env, "LOADTABLE_WHO").as_deref(), Some("old"));
        assert_eq!(
            env_or_old(&env, "LOADTABLE_MACHINE").as_deref(),
            Some("new")
        );
        assert_eq!(env_or_old(&env, "LOADTABLE_LOCK_DIR"), None);
        // other names are read as they are
        assert_eq!(env_or_old(&env, "TASKS_WHO").as_deref(), Some("old"));
        assert_eq!(env_or_old(&env, "HOME"), None);
    }
}
