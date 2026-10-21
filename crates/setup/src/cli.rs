//! Explicit read-only onboarding. This entry point never connects to the daemon.
use crate::{self as setup, PlanDocument, ProbeOptions, ProbeReport, Requirements};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USAGE: &str = "usage: tasks setup [--ui | --json] [--probe [--simulator-runtime]] [--requirements FILE] [--plan FILE]\nLoading/rendering never executes supplied commands. --probe explicitly runs known local tools.\n--ui requires its own button click for probes; installs, approvals and remote setup are not implemented.";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    ui: bool,
    json: bool,
    probe: bool,
    simulator: bool,
    requirements: Option<PathBuf>,
    plan: Option<PathBuf>,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self::default();
        let mut seen = BTreeSet::new();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            if !seen.insert(flag.as_str()) {
                return Err(format!("duplicate option {flag}; {USAGE}").into());
            }
            match flag.as_str() {
                "--ui" => options.ui = true,
                "--json" => options.json = true,
                "--probe" => options.probe = true,
                "--simulator-runtime" => options.simulator = true,
                "--requirements" | "--plan" => {
                    let value = args
                        .next()
                        .filter(|s| !s.starts_with("--"))
                        .ok_or_else(|| format!("{flag} requires a file; {USAGE}"))?;
                    let path = Some(PathBuf::from(value));
                    if flag == "--requirements" {
                        options.requirements = path;
                    } else {
                        options.plan = path;
                    }
                }
                _ => return Err(format!("unknown setup option {flag}; {USAGE}").into()),
            }
        }
        if options.ui && (options.json || options.probe || options.simulator) {
            return Err("--ui does not accept --json/--probe/--simulator-runtime; use its explicit local probe button".into());
        }
        if options.simulator && !options.probe {
            return Err(
                "--simulator-runtime requires --probe; it may start CoreSimulator services".into(),
            );
        }
        Ok(options)
    }
}

fn read_document(path: &Path) -> Result<Vec<u8>> {
    // A remote path/plan cannot make this explicit local file read follow a
    // symlink or hang opening a FIFO. Nothing here evaluates a shell string.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("setup documents must be regular files".into());
    }
    let mut bytes = Vec::new();
    file.take((setup::MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > setup::MAX_DOCUMENT_BYTES {
        return Err("setup document exceeds 256 KiB".into());
    }
    Ok(bytes)
}

pub fn run(args: &[String]) -> Result<()> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(());
    }
    let options = Options::parse(args)?;
    let received = options
        .plan
        .as_deref()
        .map(read_document)
        .transpose()?
        .map(|bytes| PlanDocument::from_json(&bytes))
        .transpose()?;
    let requirements = match options.requirements.as_deref() {
        Some(path) => Requirements::from_json(&read_document(path)?)?,
        None => received
            .as_ref()
            .map(|d| d.plan().requirements.clone())
            .unwrap_or_default(),
    };
    if options.ui {
        #[cfg(feature = "ui")]
        return super::ui::run(requirements, received).map_err(Into::into);
        #[cfg(not(feature = "ui"))]
        return Err("setup window requires a build with the ui feature".into());
    }
    let document = if options.probe {
        let report = setup::run_local_preflight(ProbeOptions {
            include_simulator_runtime: options.simulator,
        });
        setup::assess(&requirements, &report)?.document()?
    } else if let Some(received) = received {
        received
    } else {
        setup::assess(&requirements, &ProbeReport::default())?.document()?
    };
    if options.json {
        println!("{}", std::str::from_utf8(document.exact_bytes())?);
    } else {
        println!(
            "Document SHA-256: {} (contents only, not authentication or approval)",
            document.sha256()
        );
        print!("{}", document.plan().render_text()?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn default_setup_is_non_probing_and_options_require_explicit_consent() {
        assert_eq!(Options::parse(&[]).unwrap(), Options::default());
        assert!(
            Options::parse(&argv(&["--probe", "--simulator-runtime", "--json"]))
                .unwrap()
                .probe
        );
        for args in [
            vec!["--ui", "--probe"],
            vec!["--simulator-runtime"],
            vec!["--probe", "--probe"],
            vec!["--plan"],
            vec!["--plan", "--probe"],
            vec!["--install"],
            vec!["--approve"],
            vec!["--ui", "--json"],
        ] {
            assert!(Options::parse(&argv(&args)).is_err(), "{args:?}");
        }
    }
    #[test]
    fn local_document_reads_are_bounded_and_reject_links_and_non_files() {
        use std::os::unix::fs::symlink;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("macos-setup-cli-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("plan.json");
        std::fs::write(&file, b"{}").unwrap();
        assert_eq!(read_document(&file).unwrap(), b"{}");
        let link = root.join("link");
        symlink(&file, &link).unwrap();
        assert!(read_document(&link).is_err());
        assert!(read_document(&root).is_err());
        std::fs::write(&file, vec![b' '; setup::MAX_DOCUMENT_BYTES + 1]).unwrap();
        assert!(read_document(&file).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
