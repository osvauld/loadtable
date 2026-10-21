use std::io;

/// Persisted identity. The historical `start_ticks` field is kept byte-for-byte
/// compatible with existing job JSON. Treat it as an opaque host-local token:
/// Linux uses boot-relative clock ticks; macOS uses start-time microseconds
/// since the Unix epoch from PROC_PIDTBSDINFO. Never compare across machines or
/// derive elapsed time from this field without knowing its native source.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
}

struct Process {
    identity: ProcessIdentity,
    zombie: bool,
}

pub fn identity(pid: u32) -> io::Result<ProcessIdentity> {
    Ok(read(pid)?.identity)
}

/// One kernel snapshot checks both start time and zombie state. Never falls
/// back to kill(pid, 0), which cannot distinguish a reused PID.
pub fn alive(process: &ProcessIdentity) -> bool {
    read(process.pid).is_ok_and(|p| p.identity == *process && !p.zombie)
}

fn read(pid: u32) -> io::Result<Process> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process id",
        ));
    }
    native_read(pid)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(target_os = "linux")]
fn native_read(pid: u32) -> io::Result<Process> {
    parse_linux(pid, &std::fs::read_to_string(format!("/proc/{pid}/stat"))?)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux(pid: u32, stat: &str) -> io::Result<Process> {
    // comm may contain spaces, parentheses and newlines. The final ')' ends it.
    let (head, tail) = stat
        .rsplit_once(')')
        .ok_or_else(|| invalid("invalid proc stat"))?;
    let actual_pid = head
        .split_once('(')
        .and_then(|(s, _)| s.trim().parse::<u32>().ok());
    if actual_pid != Some(pid) {
        return Err(invalid("proc stat pid mismatch"));
    }
    let mut fields = tail.split_whitespace();
    let state = fields
        .next()
        .ok_or_else(|| invalid("missing process state"))?;
    let start_ticks = fields
        .nth(18)
        .ok_or_else(|| invalid("missing process start time"))?
        .parse()
        .map_err(|_| invalid("invalid process start time"))?;
    Ok(Process {
        identity: ProcessIdentity { pid, start_ticks },
        zombie: state == "Z",
    })
}

#[cfg(any(target_os = "macos", test))]
fn macos_start_token(seconds: u64, micros: u64) -> io::Result<u64> {
    if micros >= 1_000_000 {
        return Err(invalid("invalid process start microseconds"));
    }
    seconds
        .checked_mul(1_000_000)
        .and_then(|s| s.checked_add(micros))
        .ok_or_else(|| invalid("process start time overflow"))
}

#[cfg(target_os = "macos")]
fn native_read(pid: u32) -> io::Result<Process> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: this buffer has precisely the size required by PROC_PIDTBSDINFO;
    // no field is read unless libproc reports the entire structure was filled.
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize != size {
        return Err(invalid("short PROC_PIDTBSDINFO result"));
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return Err(invalid("PROC_PIDTBSDINFO pid mismatch"));
    }
    Ok(Process {
        identity: ProcessIdentity {
            pid,
            start_ticks: macos_start_token(info.pbi_start_tvsec, info.pbi_start_tvusec)?,
        },
        zombie: info.pbi_status == libc::SZOMB,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_identity_is_live_and_wrong_generation_is_not() {
        let current = identity(std::process::id()).unwrap();
        assert!(alive(&current));
        let mut reused = current.clone();
        reused.start_ticks = reused.start_ticks.wrapping_add(1);
        assert!(!alive(&reused));
        assert_eq!(identity(current.pid).unwrap(), current);
    }

    #[test]
    fn invalid_pids_are_not_process_group_queries() {
        for pid in [0, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(
                identity(pid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!alive(&ProcessIdentity {
                pid,
                start_ticks: 0
            }));
        }
    }

    #[test]
    fn historical_identity_json_is_unchanged() {
        let json = r#"{"pid":123,"start_ticks":987654321}"#;
        let value: ProcessIdentity = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&value).unwrap(), json);
    }

    #[test]
    fn proc_parser_handles_comm_and_zombies() {
        let stat = |state| {
            format!(
                "42 (tricky ) comm (\n) {state} {} 987 0 0",
                vec!["0"; 18].join(" ")
            )
        };
        let live = parse_linux(42, &stat("S")).unwrap();
        assert_eq!(live.identity.start_ticks, 987);
        assert!(!live.zombie);
        assert!(parse_linux(42, &stat("Z")).unwrap().zombie);
        assert!(parse_linux(43, &stat("S")).is_err());
        assert!(parse_linux(42, "42 (truncated) S 1").is_err());
    }

    #[test]
    fn macos_token_preserves_subsecond_generation() {
        assert_eq!(macos_start_token(123, 456).unwrap(), 123_000_456);
        assert_ne!(
            macos_start_token(123, 456).unwrap(),
            macos_start_token(123, 457).unwrap()
        );
        assert!(macos_start_token(123, 1_000_000).is_err());
        assert!(macos_start_token(u64::MAX, 0).is_err());
    }

    #[test]
    fn unreaped_zombie_is_not_alive() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        // WNOWAIT keeps the identity available but must not count as live.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let _ = child.wait();
            panic!("waitid failed: {}", io::Error::last_os_error());
        }
        let zombie = identity(pid).unwrap();
        assert!(!alive(&zombie));
        assert!(child.wait().unwrap().success());
        assert!(!alive(&zombie));
    }
}
