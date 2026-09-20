//! Changing attachment folders copies files before committing their archive paths.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::{Event, Worker};

/// Removes newly published copies on failure, but never touches the source files.
#[derive(Default)]
struct Copies(Vec<PathBuf>);

impl Drop for Copies {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Copies {
    fn keep(mut self) {
        self.0.clear();
    }

    fn copy(&mut self, source: &Path, dir: &Path) -> std::io::Result<PathBuf> {
        let name = source.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
        if source
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            == Some(dir.canonicalize()?)
        {
            return Ok(source.to_owned());
        }
        let mut input = std::fs::File::open(source)?;
        let mut staged = tempfile::NamedTempFile::new_in(dir)?;
        std::io::copy(&mut input, &mut staged)?;
        staged.as_file().sync_all()?;
        let path = publish(staged, &dir.join(name))?;
        self.0.push(path.clone());
        Ok(path)
    }
}

fn publish(mut staged: tempfile::NamedTempFile, preferred: &Path) -> std::io::Result<PathBuf> {
    let name = preferred
        .file_name()
        .ok_or(std::io::ErrorKind::InvalidInput)?;
    let mut target = preferred.to_owned();
    loop {
        match staged.persist_noclobber(&target) {
            Ok(_) => return Ok(target),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                staged = error.file;
                // Never overwrite a user's file or mistake it for this attachment.
                let mut unique =
                    std::ffi::OsString::from(format!("{:016x}-", rand::random::<u64>()));
                unique.push(name);
                target = preferred.with_file_name(unique);
            }
            Err(error) => return Err(error.error),
        }
    }
}

/// Save new downloads and uploaded attachments with the same no-clobber policy.
pub(super) async fn save(path: PathBuf, bytes: Vec<u8>) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || {
        let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
        std::fs::create_dir_all(dir)?;
        let mut staged = tempfile::NamedTempFile::new_in(dir)?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        publish(staged, &path)
    })
    .await
    .map_err(|_| "Attachment save task failed".to_owned())?
    .map_err(|error| error.to_string())
}

impl Worker {
    pub(super) async fn change_media_dir(&mut self, custom: Option<PathBuf>) {
        if let Err(error) = self.try_change_media_dir(custom).await {
            self.emit(Event::Error(format!(
                "Could not change attachment folder: {error}"
            )));
        }
    }

    async fn try_change_media_dir(&mut self, custom: Option<PathBuf>) -> Result<(), String> {
        let custom = custom.filter(|path| !self.dirs.is_default_media_dir(path));
        let dir = custom
            .clone()
            .unwrap_or_else(|| self.dirs.media_cache_dir());
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        // Resolve symlinks and '..' before checking the cache boundary.
        let dir = dir.canonicalize().map_err(|error| error.to_string())?;
        if custom.is_some() && self.dirs.is_cache_path(&dir) {
            return Err("Custom attachment folder cannot be inside the cache directory".into());
        }
        let custom = custom.map(|_| dir.clone());
        let rows = self
            .archive
            .media_paths()
            .map_err(|error| error.to_string())?;
        let (copies, updates, paths) = tokio::task::spawn_blocking(move || {
            // Even an empty archive must not accept an unwritable directory.
            let _probe = tempfile::NamedTempFile::new_in(&dir)?;
            let mut copies = Copies::default();
            let mut updates = Vec::new();
            let mut paths: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
            for (chat, id, source) in rows {
                let replacement = if let Some(path) = paths.get(&source) {
                    path.clone()
                } else {
                    let path = match source.try_exists()? {
                        true => Some(copies.copy(&source, &dir)?),
                        false => None,
                    };
                    paths.insert(source, path.clone());
                    path
                };
                updates.push((chat, id, replacement));
            }
            Ok::<_, std::io::Error>((copies, updates, paths))
        })
        .await
        .map_err(|_| "Attachment copy task failed".to_owned())?
        .map_err(|error| error.to_string())?;

        // A database failure rolls back all rows and drops only the new copies.
        self.archive
            .set_media_paths(&updates)
            .map_err(|error| error.to_string())?;
        copies.keep();
        self.dirs.custom_media = custom.clone();
        self.emit(Event::MediaDirChanged { custom, paths });
        Ok(())
    }

    /// Reconcile downloads and uploads started before the last folder change.
    pub(super) fn keep_current_media(&self, source: &Path) -> Result<PathBuf, String> {
        let dir = self
            .dirs
            .ensure_media_dir()
            .map_err(|error| error.to_string())?;
        let mut copies = Copies::default();
        let path = copies
            .copy(source, &dir)
            .map_err(|error| error.to_string())?;
        copies.keep();
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Command;
    use crate::model::{Content, Media};
    use crate::paths::AppDirs;

    const CHAT: &str = "1@s.whatsapp.net";

    fn attachment(worker: &Worker, id: &str, path: &Path) {
        worker.archive.ensure_chat(CHAT, "Fixture").unwrap();
        let mut message = crate::archive::tests::message(CHAT, id, 1, false);
        message.content = Content::Image {
            caption: None,
            media: Media {
                mime: "image/jpeg".into(),
                size: 7,
                width: None,
                height: None,
                path: Some(path.to_owned()),
                state: Default::default(),
            },
        };
        worker.archive.insert_message(&message, None).unwrap();
    }

    fn archived_path(worker: &Worker, id: &str) -> Option<PathBuf> {
        worker
            .archive
            .message(CHAT, id)
            .unwrap()
            .unwrap()
            .content
            .media()
            .unwrap()
            .path
            .clone()
    }

    #[tokio::test]
    async fn folder_changes_copy_existing_files_update_rows_and_survive_unlink_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let (mut worker, events, _commands, _wa) = super::super::receipt_tests::worker();
        worker.dirs = AppDirs::under(root.path());
        let old = worker.dirs.ensure_media_dir().unwrap().join("photo.jpg");
        std::fs::write(&old, b"fixture").unwrap();
        attachment(&worker, "image", &old);
        // Multiple forwarded rows can reference the same file.
        attachment(&worker, "forward", &old);
        let missing = old.with_file_name("missing.jpg");
        attachment(&worker, "missing", &missing);

        let custom = root.path().join("downloads");
        worker
            .handle_command(Command::SetMediaDir(custom.clone()))
            .await;
        let path = archived_path(&worker, "image").unwrap();
        assert_eq!(path.parent().unwrap(), custom.canonicalize().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"fixture");
        assert_eq!(archived_path(&worker, "forward"), Some(path.clone()));
        assert_eq!(archived_path(&worker, "missing"), None);
        assert!(
            matches!(events.try_recv().unwrap(), Event::MediaDirChanged { paths, .. }
            if paths.get(&old) == Some(&Some(path.clone())) && paths.get(&missing) == Some(&None))
        );
        worker.clean_media_cache();
        assert!(!old.exists());
        assert!(path.exists());

        worker.handle_command(Command::ResetMediaDir).await;
        assert_eq!(worker.dirs.custom_media, None);
        let reset = archived_path(&worker, "image").unwrap();
        assert_eq!(
            reset.parent().unwrap(),
            worker.dirs.media_cache_dir().canonicalize().unwrap()
        );
        assert_eq!(std::fs::read(reset).unwrap(), b"fixture");
        assert!(
            path.exists(),
            "reset preserves the user's custom folder files"
        );
    }

    #[tokio::test]
    async fn collisions_preserve_unrelated_files_and_report_the_actual_new_path() {
        let root = tempfile::tempdir().unwrap();
        let (mut worker, events, _commands, _wa) = super::super::receipt_tests::worker();
        worker.dirs = AppDirs::under(root.path());
        let old = worker.dirs.ensure_media_dir().unwrap().join("photo.jpg");
        std::fs::write(&old, b"fixture").unwrap();
        attachment(&worker, "image", &old);
        let custom = root.path().join("downloads");
        std::fs::create_dir_all(&custom).unwrap();
        let existing = custom.join("photo.jpg");
        std::fs::write(&existing, b"unrelated").unwrap();

        worker.change_media_dir(Some(custom)).await;
        let path = archived_path(&worker, "image").unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"unrelated");
        assert_eq!(std::fs::read(&path).unwrap(), b"fixture");
        assert_eq!(path.extension().unwrap(), "jpg");
        assert!(
            matches!(events.try_recv().unwrap(), Event::MediaDirChanged { paths, .. }
            if paths.get(&old) == Some(&Some(path.clone())))
        );
        assert!(old.exists());
    }

    #[tokio::test]
    async fn failed_copy_keeps_the_previous_folder_and_archive_paths() {
        let root = tempfile::tempdir().unwrap();
        let (mut worker, events, _commands, _wa) = super::super::receipt_tests::worker();
        worker.dirs = AppDirs::under(root.path());
        let old = worker.dirs.ensure_media_dir().unwrap().join("photo.jpg");
        std::fs::write(&old, b"fixture").unwrap();
        attachment(&worker, "image", &old);
        let invalid = old.with_file_name("directory.jpg");
        std::fs::create_dir_all(&invalid).unwrap();
        attachment(&worker, "invalid", &invalid);

        let custom = root.path().join("downloads");
        worker.change_media_dir(Some(custom.clone())).await;
        assert_eq!(worker.dirs.custom_media, None);
        assert_eq!(archived_path(&worker, "image"), Some(old.clone()));
        assert_eq!(archived_path(&worker, "invalid"), Some(invalid));
        assert_eq!(std::fs::read(&old).unwrap(), b"fixture");
        assert_eq!(std::fs::read_dir(custom).unwrap().count(), 0);
        assert!(matches!(events.try_recv().unwrap(), Event::Error(_)));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn invalid_or_cache_folders_are_rejected_and_default_selection_resets() {
        let root = tempfile::tempdir().unwrap();
        let (mut worker, events, _commands, _wa) = super::super::receipt_tests::worker();
        worker.dirs = AppDirs::under(root.path());
        worker.dirs.ensure().unwrap();
        let blocked = root.path().join("file");
        std::fs::write(&blocked, b"fixture").unwrap();
        for path in [blocked, worker.dirs.cache.join("nested/../forbidden")] {
            worker.change_media_dir(Some(path)).await;
            assert!(matches!(events.try_recv().unwrap(), Event::Error(_)));
            assert_eq!(worker.dirs.custom_media, None);
        }
        worker
            .change_media_dir(Some(root.path().join("downloads")))
            .await;
        assert!(worker.dirs.custom_media.is_some());
        worker
            .change_media_dir(Some(worker.dirs.media_cache_dir()))
            .await;
        assert_eq!(worker.dirs.custom_media, None);
    }

    #[tokio::test]
    async fn a_download_started_in_the_old_folder_finishes_in_the_current_folder() {
        let root = tempfile::tempdir().unwrap();
        let (mut worker, events, _commands, _wa) = super::super::receipt_tests::worker();
        worker.dirs = AppDirs::under(root.path());
        let old = worker.dirs.ensure_media_dir().unwrap().join("late.jpg");
        attachment(&worker, "image", &old);
        worker
            .change_media_dir(Some(root.path().join("downloads")))
            .await;
        let _ = events.try_recv().unwrap();
        std::fs::write(&old, b"fixture").unwrap();
        worker
            .handle_command(Command::Downloaded {
                chat: CHAT.into(),
                id: "image".into(),
                result: Ok(old.clone()),
            })
            .await;
        let path = archived_path(&worker, "image").unwrap();
        assert_eq!(path.parent().unwrap(), worker.dirs.media_dir());
        assert!(
            matches!(events.try_recv().unwrap(), Event::Media { result: Ok(result), .. } if result == path)
        );
        worker.clean_media_cache();
        assert_eq!(std::fs::read(path).unwrap(), b"fixture");
    }

    #[test]
    fn failed_operation_removes_only_new_copies() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.jpg");
        std::fs::write(&source, b"fixture").unwrap();
        let dir = root.path().join("target");
        std::fs::create_dir_all(&dir).unwrap();
        let mut copies = Copies::default();
        let target = copies.copy(&source, &dir).unwrap();
        assert!(target.exists());
        drop(copies);
        assert!(!target.exists());
        assert!(source.exists());
    }

    #[tokio::test]
    async fn new_downloads_do_not_overwrite_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let preferred = root.path().join("downloads/photo.jpg");
        let first = save(preferred.clone(), b"first".to_vec()).await.unwrap();
        let second = save(preferred.clone(), b"second".to_vec()).await.unwrap();
        assert_eq!(first, preferred);
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
    }

    #[test]
    fn legacy_custom_cache_subtrees_survive_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let (mut worker, _events, _commands, _wa) = super::super::receipt_tests::worker();
        worker.dirs = AppDirs::under(root.path());
        let cache = worker.dirs.ensure_media_dir().unwrap();
        let custom = cache.join("legacy/custom");
        std::fs::create_dir_all(&custom).unwrap();
        let saved = custom.join("saved.jpg");
        std::fs::write(&saved, b"fixture").unwrap();
        let disposable = cache.join("cached.jpg");
        std::fs::write(&disposable, b"fixture").unwrap();
        worker.dirs.custom_media = Some(custom);
        worker.clean_media_cache();
        assert!(saved.exists());
        assert!(!disposable.exists());
        worker.dirs.custom_media = Some(cache);
        worker.clean_media_cache();
        assert!(saved.exists());
    }
}
