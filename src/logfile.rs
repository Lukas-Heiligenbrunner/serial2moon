//! Size-capped log file with gzip-compressed history.
//!
//! The active log keeps its plain name (`serial2moon.log`, which Mainsail lists and the
//! docs point to). Once it would exceed `max_bytes` it is compressed to `<name>.1.gz`,
//! older archives shift up (`.1.gz` → `.2.gz` …), and only the newest `keep` are kept.
//! Debug logging writes ~23 MB per print hour; gzip shrinks it ~5-6x.
//!
//! Rotation runs inline on the caller's thread — the non-blocking tracing worker, so the
//! serial path never waits on it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: File,
    /// Bytes in the active file, including what was there before we opened it.
    len: u64,
}

impl RotatingFile {
    /// Open (append to) the log at `path`, creating its directory if needed. Archives
    /// beyond `keep` (e.g. after the limit was lowered) are removed right away.
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata()?.len();
        let log = RotatingFile {
            path,
            max_bytes,
            keep,
            file,
            len,
        };
        log.prune();
        Ok(log)
    }

    fn archive(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}.gz"));
        PathBuf::from(name)
    }

    /// Delete `<name>.<n>.gz` archives with `n > keep`.
    fn prune(&self) {
        let (Some(dir), Some(name)) = (self.path.parent(), self.path.file_name()) else {
            return;
        };
        let prefix = format!("{}.", name.to_string_lossy());
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let n = file_name
                .to_str()
                .and_then(|f| f.strip_prefix(&prefix))
                .and_then(|f| f.strip_suffix(".gz"))
                .and_then(|n| n.parse::<usize>().ok());
            if n.is_some_and(|n| n > self.keep) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Archive the active file as `.1.gz` (shifting older archives up, dropping the
    /// oldest) and start a fresh one. If archiving fails the active file is still
    /// truncated: staying within the size cap matters more than that history.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        if self.keep > 0
            && let Err(e) = self.archive_active()
        {
            eprintln!(
                "serial2moon: could not archive {}: {e}",
                self.path.display()
            );
        }
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.len = 0;
        Ok(())
    }

    fn archive_active(&self) -> io::Result<()> {
        // Compress to a temp name first, so a crash mid-way never leaves a truncated .1.gz.
        let tmp = PathBuf::from(format!("{}.tmp", self.archive(1).display()));
        if let Err(e) = gzip(&self.path, &tmp) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        for n in (1..self.keep).rev() {
            let from = self.archive(n);
            if from.exists() {
                fs::rename(&from, self.archive(n + 1))?; // replaces (drops) the oldest
            }
        }
        fs::rename(&tmp, self.archive(1))
    }
}

fn gzip(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = BufReader::new(File::open(src)?);
    let mut encoder = GzEncoder::new(BufWriter::new(File::create(dst)?), Compression::default());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?.flush()
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotate between writes (the tracing worker writes one whole event per call), never
        // when empty — a single oversized event still lands in a fresh file.
        if self.len > 0 && self.len + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::Path;

    use flate2::read::GzDecoder;

    use super::*;

    /// A fresh, empty directory per test (removed again on drop).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("serial2moon-logfile-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn log(&self) -> PathBuf {
            self.0.join("serial2moon.log")
        }
        fn archive(&self, n: usize) -> PathBuf {
            self.0.join(format!("serial2moon.log.{n}.gz"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn gunzip(path: &Path) -> String {
        let mut s = String::new();
        GzDecoder::new(File::open(path).unwrap())
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn rotates_into_a_compressed_archive_at_the_size_limit() {
        let dir = TempDir::new("rotate");
        let mut log = RotatingFile::open(dir.log(), 20, 3).unwrap();
        log.write_all(b"first line\n").unwrap(); // 11 bytes
        log.write_all(b"second line\n").unwrap(); // would reach 23 > 20 -> rotate first
        log.flush().unwrap();

        assert_eq!(gunzip(&dir.archive(1)), "first line\n");
        assert_eq!(read(&dir.log()), "second line\n");
    }

    #[test]
    fn keeps_only_the_newest_archives() {
        let dir = TempDir::new("keep");
        let mut log = RotatingFile::open(dir.log(), 5, 2).unwrap();
        for line in ["one\n", "two\n", "three\n", "four\n"] {
            log.write_all(line.as_bytes()).unwrap(); // every line rotates the previous one
        }
        log.flush().unwrap();

        assert_eq!(read(&dir.log()), "four\n");
        assert_eq!(gunzip(&dir.archive(1)), "three\n");
        assert_eq!(gunzip(&dir.archive(2)), "two\n");
        assert!(!dir.archive(3).exists(), "oldest archive dropped");
    }

    #[test]
    fn rotates_an_existing_oversized_log_on_first_write() {
        // E.g. the multi-GB log left behind by versions without retention.
        let dir = TempDir::new("legacy");
        std::fs::write(dir.log(), "x".repeat(100)).unwrap();
        let mut log = RotatingFile::open(dir.log(), 50, 3).unwrap();
        log.write_all(b"fresh\n").unwrap();
        log.flush().unwrap();

        assert_eq!(gunzip(&dir.archive(1)), "x".repeat(100));
        assert_eq!(read(&dir.log()), "fresh\n");
    }

    #[test]
    fn appends_to_an_existing_log_below_the_limit() {
        let dir = TempDir::new("append");
        std::fs::write(dir.log(), "earlier\n").unwrap();
        let mut log = RotatingFile::open(dir.log(), 1000, 3).unwrap();
        log.write_all(b"later\n").unwrap();
        log.flush().unwrap();

        assert_eq!(read(&dir.log()), "earlier\nlater\n");
        assert!(!dir.archive(1).exists());
    }

    #[test]
    fn removes_archives_beyond_a_lowered_limit() {
        let dir = TempDir::new("prune");
        for n in 1..=5 {
            std::fs::write(dir.archive(n), b"old").unwrap();
        }
        let _log = RotatingFile::open(dir.log(), 1000, 2).unwrap();

        assert!(dir.archive(1).exists() && dir.archive(2).exists());
        assert!((3..=5).all(|n| !dir.archive(n).exists()));
    }

    #[test]
    fn keep_zero_truncates_without_history() {
        let dir = TempDir::new("nohistory");
        let mut log = RotatingFile::open(dir.log(), 5, 0).unwrap();
        log.write_all(b"one\n").unwrap();
        log.write_all(b"two\n").unwrap();
        log.flush().unwrap();

        assert_eq!(read(&dir.log()), "two\n");
        assert!(!dir.archive(1).exists());
    }
}
