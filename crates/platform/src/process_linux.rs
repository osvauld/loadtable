use super::{Identity, Observation};
#[cfg(target_os = "linux")]
use std::fs;
use std::io;

#[cfg(target_os = "linux")]
pub(super) fn observe(pid: u32) -> io::Result<Observation> {
    parse(pid, &fs::read_to_string(format!("/proc/{pid}/stat"))?)
}

fn parse(pid: u32, stat: &str) -> io::Result<Observation> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid proc stat identity");
    // comm may contain spaces and parentheses; state begins after the final ')'.
    let rest = stat.rsplit_once(')').ok_or_else(invalid)?.1;
    let mut fields = rest.split_whitespace();
    let zombie = fields.next().ok_or_else(invalid)? == "Z";
    let start_ticks = fields
        .nth(18)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    Ok(Observation {
        identity: Identity { pid, start_ticks },
        zombie,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_parentheses_and_zombie_state_do_not_shift_start_ticks() {
        for state in ["S", "Z"] {
            let mut fields = vec![state.to_owned()];
            fields.extend((1..19).map(|_| "0".to_owned()));
            fields.push("123456".to_owned());
            let observed = parse(42, &format!("42 (nested (name)) {}", fields.join(" "))).unwrap();
            assert_eq!(observed.identity.start_ticks, 123456);
            assert_eq!(observed.zombie, state == "Z");
        }
        assert!(parse(42, "malformed").is_err());
        assert!(parse(42, "42 (name) S 0").is_err());
    }
}
