//! Captured answers outlive the child that produced them.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;
use uuid::Uuid;

use super::{Envelope, Failure, FailureKind};
use crate::supervise::cleanup::{create_private_directory, temp_root};

pub const CAPTURE_LIMIT: usize = 4 << 20;
const EXCERPT: usize = 16 << 10;
const KEEP: usize = 8;

/// Created on first use. Readers have access until the eighth newer answer or server exit.
#[derive(Clone, Default)]
pub struct AnswerArchive(Arc<Mutex<Option<Directory>>>);

struct Directory {
    path: PathBuf,
    files: VecDeque<PathBuf>,
}

impl Drop for Directory {
    fn drop(&mut self) {
        for path in &self.files {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&self.path);
    }
}

impl AnswerArchive {
    fn save(&self, answer: &str) -> io::Result<String> {
        let mut slot = self.0.lock().unwrap();
        if slot.is_none() {
            let path = temp_root()?.join(format!("proxenos-answers-{}", Uuid::new_v4()));
            create_private_directory(&path)?;
            *slot = Some(Directory {
                path,
                files: VecDeque::new(),
            });
        }
        let dir = slot.as_mut().unwrap();
        if dir.files.len() == KEEP {
            match fs::remove_file(&dir.files[0]) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                result => result?,
            }
            dir.files.pop_front();
        }
        let path = dir.path.join(format!("{}.txt", Uuid::new_v4()));
        let json_path = path.to_str().ok_or(io::ErrorKind::InvalidData)?.to_owned();
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&path)?;
        // A partial write still counts towards the limit and gets removed at shutdown.
        dir.files.push_back(path.clone());
        file.write_all(answer.as_bytes())?;
        Ok(json_path)
    }
}

impl Envelope {
    async fn retain(
        &mut self,
        archive: &AnswerArchive,
        prefix: &str,
        text: &mut String,
        report_capture_as_failure: bool,
    ) {
        if text.len() <= EXCERPT {
            return;
        }
        let captured = std::mem::take(text);
        self.observed[format!("{prefix}_captured_bytes")] = json!(captured.len());
        *text = captured[..captured.floor_char_boundary(EXCERPT)].to_owned();
        let bytes = captured.floor_char_boundary(CAPTURE_LIMIT);
        let archive = archive.clone();
        let saved = tokio::task::spawn_blocking(move || archive.save(&captured[..bytes]))
            .await
            .expect("answer writer completed");
        match saved {
            Ok(path) => {
                self.observed[format!("{prefix}_file")] = json!(path);
                self.observed[format!("{prefix}_file_bytes")] = json!(bytes);
            }
            Err(error) => {
                let failure = Failure::new(
                    FailureKind::Capture,
                    format!("Could not retain the captured {prefix}: {error}"),
                );
                self.observed["capture_error"] = json!(failure);
                if report_capture_as_failure {
                    self.fail(failure);
                }
            }
        }
    }

    pub async fn excerpt(&mut self, archive: &AnswerArchive) {
        if let Some(mut content) = self.report.content.take() {
            let no_turn_failure = self.report.failure.is_none();
            self.retain(archive, "content", &mut content, no_turn_failure)
                .await;
            self.report.content = Some(content);
        }
        if let Some(mut failure) = self.report.failure.take() {
            self.retain(archive, "failure", &mut failure.message, false)
                .await;
            if let Some(reset) = &mut failure.reset_at {
                reset.truncate(reset.floor_char_boundary(EXCERPT));
            }
            self.report.failure = Some(failure);
        }
    }
}
