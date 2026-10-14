//! Follows Game.log as it grows, like `tail -f`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::SystemTime;

pub struct LogTail {
    path: PathBuf,
    pos: u64,
    created: Option<SystemTime>,
    partial: Vec<u8>,
}

pub struct Poll {
    /// The game was restarted and replaced the log since the last poll.
    pub reset: bool,
    pub lines: Vec<String>,
}

impl LogTail {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            pos: 0,
            created: None,
            partial: Vec::new(),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Reads any complete lines appended since the last poll.
    pub fn poll(&mut self) -> io::Result<Poll> {
        let mut file = File::open(&self.path)?;
        let meta = file.metadata()?;
        let created = meta.created().ok();

        let replaced = self.created.is_some() && created != self.created;
        let reset = replaced || meta.len() < self.pos;
        if reset {
            self.pos = 0;
            self.partial.clear();
        }
        self.created = created;

        let mut lines = Vec::new();
        if meta.len() == self.pos {
            return Ok(Poll { reset, lines });
        }

        file.seek(SeekFrom::Start(self.pos))?;
        let read = file.take(meta.len() - self.pos).read_to_end(&mut self.partial)?;
        self.pos += read as u64;

        // Keep a trailing partial line until the game finishes writing it.
        if let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') {
            let complete: Vec<u8> = self.partial.drain(..=end).collect();
            lines = String::from_utf8_lossy(&complete)
                .lines()
                .map(|l| l.trim_end().to_string())
                .collect();
        }
        Ok(Poll { reset, lines })
    }
}
