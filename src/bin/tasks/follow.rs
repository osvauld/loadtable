//! Client-side log streaming. The worker appends to `STATE/jobs/ID/job.log` (same
//! user, bounded size); this reads new bytes by offset so `--follow` needs no
//! protocol change and never replays or mutates anything.
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bytes read per poll; the rest is picked up on the next poll.
const CHUNK: u64 = 1024 * 1024;

pub fn job_log(state: &Path, id: i64) -> PathBuf {
    state.join("jobs").join(id.to_string()).join("job.log")
}

pub struct LogFollower {
    path: PathBuf,
    offset: u64,
}

impl LogFollower {
    pub fn new(path: PathBuf) -> Self {
        Self { path, offset: 0 }
    }
    /// Bytes read so far; zero means nothing has been streamed.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// New bytes since the last call. A log that does not exist yet reads as empty.
    pub fn poll(&mut self) -> std::io::Result<Vec<u8>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Logs are append-only; a shorter file is a different file. Start over.
            self.offset = 0;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(CHUNK).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        Ok(bytes)
    }
    /// Everything up to the current end, in chunks.
    pub fn drain(&mut self) -> std::io::Result<Vec<u8>> {
        let mut all = Vec::new();
        loop {
            let bytes = self.poll()?;
            if bytes.is_empty() {
                return Ok(all);
            }
            all.extend(bytes);
        }
    }
}

/// Splits streamed bytes into complete lines, keeping a partial line for later.
#[derive(Default)]
pub struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(bytes);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        String::from_utf8_lossy(&complete)
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// A progress line worth showing while a long job runs without `--follow`.
pub fn progress(line: &str) -> Option<&str> {
    line.strip_prefix("Android step: ").map(str::trim)
}

/// The last `lines` lines of `text` and how many earlier lines were left out.
pub fn tail(text: &str, lines: usize) -> (&str, usize) {
    let total = text.lines().count();
    if total <= lines {
        return (text, 0);
    }
    let skip = total - lines;
    let start = text
        .match_indices('\n')
        .nth(skip - 1)
        .map(|(i, _)| i + 1)
        .unwrap_or(0);
    (&text[start..], skip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn follower_streams_appended_bytes_once_in_order() {
        let dir = std::env::temp_dir().join(format!("tasks-follow-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("job.log");
        let _ = std::fs::remove_file(&path);
        let mut follower = LogFollower::new(path.clone());
        // Before the worker creates the log: nothing, no error.
        assert!(follower.poll().unwrap().is_empty());
        let mut log = File::options()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        log.write_all(b"line 1\npart").unwrap();
        assert_eq!(follower.poll().unwrap(), b"line 1\npart");
        assert!(follower.poll().unwrap().is_empty());
        log.write_all(b"ial\nline 3\n").unwrap();
        assert_eq!(follower.drain().unwrap(), b"ial\nline 3\n");
        assert_eq!(follower.offset(), 22);
        // Large appends arrive across polls without loss.
        let big = vec![b'x'; (CHUNK + 10) as usize];
        log.write_all(&big).unwrap();
        assert_eq!(follower.poll().unwrap().len() as u64, CHUNK);
        assert_eq!(follower.drain().unwrap().len(), 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_wait_for_newlines() {
        let mut lines = Lines::default();
        assert!(lines.push(b"Android st").is_empty());
        assert_eq!(lines.push(b"ep: gradle\nnext"), ["Android step: gradle"]);
        assert_eq!(lines.push(b" line\n\nlast\n"), ["next line", "", "last"]);
    }

    #[test]
    fn progress_shows_android_steps_only() {
        assert_eq!(
            progress("Android step: cargo-ndk arm64-v8a"),
            Some("cargo-ndk arm64-v8a")
        );
        assert_eq!(progress("   Compiling foo v1"), None);
    }

    #[test]
    fn tail_bounds_the_completion_output() {
        let text: String = (1..=650).map(|i| format!("line {i}\n")).collect();
        let (shown, omitted) = tail(&text, 40);
        assert_eq!(omitted, 610);
        assert_eq!(shown.lines().count(), 40);
        assert!(shown.starts_with("line 611\n") && shown.ends_with("line 650\n"));
        assert_eq!(tail("a\nb", 5), ("a\nb", 0));
        assert_eq!(tail("a\nb\nc", 2), ("b\nc", 1));
        assert_eq!(tail("", 2), ("", 0));
    }
}
