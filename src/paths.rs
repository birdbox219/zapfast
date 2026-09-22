//! Where ZapFast keeps its files.
//!
//! Configuration, session state, and caches use separate standard platform
//! directories. Clearing a cache does not remove device keys.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

#[derive(Clone, Debug)]
pub struct AppDirs {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub custom_media: Option<PathBuf>,
}

impl AppDirs {
    pub fn discover() -> Self {
        match Self::of("zapfast") {
            Some(dirs) => dirs,
            None => {
                let fallback = std::env::current_dir().unwrap_or_default();
                Self {
                    config: fallback.join("zapfast-config"),
                    state: fallback.join("zapfast-state"),
                    cache: fallback.join("zapfast-cache"),
                    custom_media: None,
                }
            }
        }
    }

    /// Standard platform directories for the app.
    fn of(name: &str) -> Option<Self> {
        let project = ProjectDirs::from("me", "paolino", name)?;
        Some(Self {
            config: project.config_dir().to_path_buf(),
            state: project
                .state_dir()
                .map(|path| path.to_path_buf())
                .unwrap_or_else(|| project.data_local_dir().to_path_buf()),
            cache: project.cache_dir().to_path_buf(),
            custom_media: None,
        })
    }

    /// Adopts earlier names, newest first, without replacing existing data.
    /// Call only after acquiring the instance guard, and never for demo runs.
    pub fn adopt_previous_names(&self) -> std::io::Result<()> {
        for name in ["fastsapp", "fastwhatsapp"] {
            if let Some(old) = Self::of(name) {
                self.adopt(&old)?;
            }
            if let (Some(from), Some(to)) =
                (eframe::storage_dir(name), eframe::storage_dir("zapfast"))
            {
                adopt_directory(&from, &to)?;
            }
        }
        Ok(())
    }

    fn adopt(&self, old: &Self) -> std::io::Result<()> {
        for (from, to) in [
            (&old.config, &self.config),
            (&old.state, &self.state),
            (&old.cache, &self.cache),
        ] {
            adopt_directory(from, to)?;
        }
        Ok(())
    }

    /// Places all data under one directory for tests and temporary runs.
    pub fn under(root: &std::path::Path) -> Self {
        Self {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
            custom_media: None,
        }
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }

    /// whatsapp-rust device identity, Signal sessions, and state keys.
    /// Deleting this database unlinks the computer.
    pub fn session_db(&self) -> PathBuf {
        self.state.join("session.db")
    }

    /// Local message archive.
    pub fn archive_db(&self) -> PathBuf {
        self.state.join("archive.db")
    }

    /// Current-run log, replaced at startup.
    pub fn log_file(&self) -> PathBuf {
        self.state.join("zapfast.log")
    }

    /// Panic log written before process exit.
    pub fn panic_log(&self) -> PathBuf {
        self.state.join("panic.log")
    }

    /// Downloaded attachments keyed by message id.
    pub fn media_cache_dir(&self) -> PathBuf {
        self.cache.join("media")
    }

    /// Effective attachment directory, using a custom path when configured.
    pub fn media_dir(&self) -> PathBuf {
        self.custom_media
            .clone()
            .unwrap_or_else(|| self.media_cache_dir())
    }

    /// Creates the effective attachment folder before opening it in the desktop.
    pub fn ensure_media_dir(&self) -> std::io::Result<PathBuf> {
        let dir = self.media_dir();
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Returns true if `path` resolves to the default media cache directory.
    pub fn is_default_media_dir(&self, path: &Path) -> bool {
        let default = self.media_cache_dir();
        match (path.canonicalize(), default.canonicalize()) {
            (Ok(p), Ok(d)) => p == d,
            _ => path == default,
        }
    }

    /// Returns true if `path` is equal to or contained within `self.cache`.
    pub fn is_cache_path(&self, path: &Path) -> bool {
        Self::is_subpath(path, &self.cache)
    }

    /// Validate without creating folders or requiring external storage to be online.
    pub fn validate_custom_media_dir(&self, path: &Path) -> std::io::Result<Option<PathBuf>> {
        let path = resolve_available_parents(path)?;
        if path == resolve_available_parents(&self.media_cache_dir())? {
            return Ok(None);
        }
        for (boundary, message) in [
            (
                &self.cache,
                "Custom attachment folder cannot be inside the cache directory",
            ),
            (
                &self.state,
                "Custom attachment folder cannot be inside the app data folders",
            ),
            (
                &self.config,
                "Custom attachment folder cannot be inside the app data folders",
            ),
        ] {
            if path.starts_with(resolve_available_parents(boundary)?) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    message,
                ));
            }
        }
        Ok(Some(path))
    }

    /// Checks whether `child` is equal to or located within `parent`.
    pub fn is_subpath(child: &Path, parent: &Path) -> bool {
        match (child.canonicalize(), parent.canonicalize()) {
            (Ok(c), Ok(p)) => c.starts_with(&p),
            _ => child.starts_with(parent),
        }
    }

    /// Profile pictures keyed by chat.
    pub fn avatar_cache_dir(&self) -> PathBuf {
        self.cache.join("avatars")
    }

    /// Recent phone stickers keyed by file hash.
    pub fn sticker_cache_dir(&self) -> PathBuf {
        self.cache.join("stickers")
    }

    /// Saved stickers keyed by content hash. These are user data, not cache.
    pub fn saved_sticker_dir(&self) -> PathBuf {
        self.state.join("stickers")
    }

    /// Cached profile-picture path. `full` selects the info-dialog size.
    pub fn avatar_file(&self, id: &str, full: bool) -> PathBuf {
        let stem: String = id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        self.avatar_cache_dir()
            .join(format!("{stem}{}.jpg", if full { "-full" } else { "" }))
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [&self.config, &self.state, &self.cache] {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            // Create new directories privately, even with a permissive umask.
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(dir)?;
            restrict_directory(dir)?;
        }
        Ok(())
    }
}

// Resolve existing symlinks before processing '..', but keep unavailable suffixes.
fn resolve_available_parents(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            _ => {
                resolved.push(component.as_os_str());
                if let Ok(canonical) = resolved.canonicalize() {
                    resolved = canonical;
                }
            }
        }
    }
    Ok(resolved)
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Rename whole directories so SQLite databases travel with their WAL files.
/// A failed move stops startup before empty replacement directories are made.
fn adopt_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() && !to.try_exists()? {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(from, to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("zapfast-paths-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn ensure_restricts_base_directories() {
        use std::os::unix::fs::PermissionsExt;

        let root = root("permissions");
        let dirs = AppDirs::under(&root);
        dirs.ensure().unwrap();
        for path in [&dirs.config, &dirs.state, &dirs.cache] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_repairs_existing_directory_permissions_without_changing_data() {
        use std::os::unix::fs::PermissionsExt;

        let root = root("existing-permissions");
        let dirs = AppDirs::under(&root);
        for path in [&dirs.config, &dirs.state, &dirs.cache] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(path.join("fixture"), b"preserved").unwrap();
        }
        dirs.ensure().unwrap();
        for path in [&dirs.config, &dirs.state, &dirs.cache] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(std::fs::read(path.join("fixture")).unwrap(), b"preserved");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ensure_stops_when_an_application_directory_cannot_be_created() {
        let root = root("blocked-directory");
        let dirs = AppDirs::under(&root);
        std::fs::write(&dirs.state, b"existing file").unwrap();
        assert!(dirs.ensure().is_err());
        assert!(!dirs.cache.exists());
        assert_eq!(std::fs::read(&dirs.state).unwrap(), b"existing file");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_preserves_session_archive_settings_and_cached_files() {
        for name in ["fastsapp", "fastwhatsapp"] {
            let root = root(name);
            let old = AppDirs::under(&root.join(name));
            let new = AppDirs::under(&root.join("zapfast"));
            old.ensure().unwrap();
            for path in [
                old.settings_file(),
                old.session_db(),
                old.state.join("session.db-wal"),
                old.archive_db(),
                old.state.join("archive.db-wal"),
                old.saved_sticker_dir().join("pack/sticker.webp"),
                old.media_cache_dir().join("photo.jpg"),
            ] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"preserved").unwrap();
            }
            new.adopt(&old).unwrap();
            new.adopt(&old).unwrap(); // A second launch is a no-op.
            for path in [
                new.settings_file(),
                new.session_db(),
                new.state.join("session.db-wal"),
                new.archive_db(),
                new.state.join("archive.db-wal"),
                new.saved_sticker_dir().join("pack/sticker.webp"),
                new.media_cache_dir().join("photo.jpg"),
            ] {
                assert_eq!(std::fs::read(path).unwrap(), b"preserved");
            }
            assert!(!old.config.exists());
            assert!(!old.state.exists());
            assert!(!old.cache.exists());
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn newest_data_wins_without_merging_archives() {
        let root = root("precedence");
        let new = AppDirs::under(&root.join("zapfast"));
        let recent = AppDirs::under(&root.join("fastsapp"));
        let oldest = AppDirs::under(&root.join("fastwhatsapp"));
        recent.ensure().unwrap();
        oldest.ensure().unwrap();
        std::fs::create_dir_all(&new.config).unwrap();
        std::fs::write(new.settings_file(), b"new settings").unwrap();
        std::fs::write(recent.settings_file(), b"old settings").unwrap();
        std::fs::write(recent.archive_db(), b"recent archive").unwrap();
        std::fs::write(oldest.archive_db(), b"oldest archive").unwrap();
        new.adopt(&recent).unwrap();
        new.adopt(&oldest).unwrap();
        assert_eq!(std::fs::read(new.settings_file()).unwrap(), b"new settings");
        assert_eq!(
            std::fs::read(recent.settings_file()).unwrap(),
            b"old settings"
        );
        assert_eq!(std::fs::read(new.archive_db()).unwrap(), b"recent archive");
        assert_eq!(
            std::fs::read(oldest.archive_db()).unwrap(),
            b"oldest archive"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_config_and_state_directory_moves_once() {
        let root = root("shared");
        let old = AppDirs {
            config: root.join("old/data"),
            state: root.join("old/data"),
            cache: root.join("old/cache"),
            custom_media: None,
        };
        let new = AppDirs {
            config: root.join("new/data"),
            state: root.join("new/data"),
            cache: root.join("new/cache"),
            custom_media: None,
        };
        old.ensure().unwrap();
        std::fs::write(old.session_db(), b"session").unwrap();
        new.adopt(&old).unwrap();
        assert_eq!(std::fs::read(new.session_db()).unwrap(), b"session");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn media_dir_uses_custom_path_when_configured() {
        let root = root("custom-media");
        let mut dirs = AppDirs::under(&root);
        assert_eq!(dirs.media_dir(), dirs.media_cache_dir());
        let custom = root.join("my-custom-media");
        dirs.custom_media = Some(custom.clone());
        assert_eq!(dirs.media_dir(), custom);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_path_detection_identifies_subpaths() {
        let root = root("cache-detection");
        let dirs = AppDirs::under(&root);
        dirs.ensure().unwrap();

        assert!(dirs.is_default_media_dir(&dirs.media_cache_dir()));
        assert!(dirs.is_cache_path(&dirs.media_cache_dir()));
        assert!(dirs.is_cache_path(&dirs.media_cache_dir().join("subfolder")));
        assert!(!dirs.is_cache_path(&root.join("external-downloads")));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn custom_media_validation_handles_missing_children_and_default_aliases() {
        let root = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(root.path());
        dirs.ensure().unwrap();
        assert_eq!(
            dirs.validate_custom_media_dir(&dirs.cache.join("unused/../media"))
                .unwrap(),
            None
        );
        for boundary in [&dirs.cache, &dirs.state, &dirs.config] {
            assert!(
                dirs.validate_custom_media_dir(&boundary.join("missing/child"))
                    .is_err()
            );
            assert!(!boundary.join("missing").exists());
        }
        assert_eq!(
            dirs.validate_custom_media_dir(root.path()).unwrap(),
            Some(root.path().canonicalize().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_media_validation_resolves_symlinks_before_missing_children_and_parent_steps() {
        let root = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(root.path());
        dirs.ensure().unwrap();
        let link = root.path().join("alias");
        std::os::unix::fs::symlink(&dirs.state, &link).unwrap();
        assert!(
            dirs.validate_custom_media_dir(&link.join("missing"))
                .is_err()
        );
        let nested = dirs.state.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let nested_link = root.path().join("nested-alias");
        std::os::unix::fs::symlink(nested, &nested_link).unwrap();
        assert!(
            dirs.validate_custom_media_dir(&nested_link.join("../missing"))
                .is_err()
        );
    }

    #[test]
    fn opening_media_folder_creates_it_before_and_after_cache_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let mut dirs = AppDirs::under(root.path());
        dirs.ensure().unwrap();
        assert!(!dirs.media_dir().exists());
        assert!(dirs.ensure_media_dir().unwrap().is_dir());
        std::fs::remove_dir_all(dirs.media_cache_dir()).unwrap();
        assert!(dirs.ensure_media_dir().unwrap().is_dir());

        dirs.custom_media = Some(root.path().join("custom"));
        assert!(dirs.ensure_media_dir().unwrap().is_dir());
        let blocked = root.path().join("file");
        std::fs::write(&blocked, b"fixture").unwrap();
        dirs.custom_media = Some(blocked.clone());
        assert!(dirs.ensure_media_dir().is_err());
        assert_eq!(std::fs::read(blocked).unwrap(), b"fixture");
    }

    #[test]
    fn failed_migration_leaves_source_available_for_retry() {
        let root = root("failure");
        let old = AppDirs::under(&root.join("old"));
        let new = AppDirs::under(&root.join("blocked/new"));
        old.ensure().unwrap();
        std::fs::write(old.session_db(), b"session").unwrap();
        std::fs::write(root.join("blocked"), b"not a directory").unwrap();
        assert!(new.adopt(&old).is_err());
        assert_eq!(std::fs::read(old.session_db()).unwrap(), b"session");
        std::fs::remove_file(root.join("blocked")).unwrap();
        new.adopt(&old).unwrap();
        assert_eq!(std::fs::read(new.session_db()).unwrap(), b"session");
        std::fs::remove_dir_all(root).unwrap();
    }
}
