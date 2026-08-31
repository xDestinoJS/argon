use crossbeam_channel::Receiver;
use std::{
	fs,
	fs::OpenOptions,
	io::Write,
	io::{Error, Result},
	path::{Path, PathBuf},
};
use uuid::Uuid;

use super::{debouncer::VfsDebouncer, VfsBackend, VfsEvent, VfsPathKind};
use crate::config::Config;

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
	// Keep the temporary name shorter than common metadata filenames. Appending a
	// full UUID to `init.meta.json` pushed deeply nested duplicate-instance paths
	// past Windows' path limit even when the destination itself was writable.
	let token = Uuid::new_v4().simple().to_string();
	let temp_path = path.with_file_name(format!(".a{}", &token[..8]));

	let result = (|| -> Result<()> {
		let mut temp = OpenOptions::new().create_new(true).write(true).open(&temp_path)?;
		temp.write_all(contents)?;
		temp.sync_all()?;
		drop(temp);

		for attempt in 0..5 {
			match atomic_replace(&temp_path, path) {
				Ok(()) => return Ok(()),
				Err(err) if attempt < 4 => {
					std::thread::sleep(std::time::Duration::from_millis(2));
					if !temp_path.exists() {
						return Err(err);
					}
				}
				Err(err) => return Err(err),
			}
		}

		unreachable!()
	})();

	if result.is_err() {
		let _ = fs::remove_file(&temp_path);
	}
	result
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
	use std::os::windows::ffi::OsStrExt;
	use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

	let from = from.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
	let to = to.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
	let success = unsafe {
		MoveFileExW(
			from.as_ptr(),
			to.as_ptr(),
			MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
		)
	};

	if success == 0 {
		Err(Error::last_os_error())
	} else {
		Ok(())
	}
}

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<()> {
	fs::rename(from, to)
}

pub struct StdBackend {
	watching: bool,
	debouncer: VfsDebouncer,
	watched_paths: Vec<PathBuf>,
}

impl StdBackend {
	pub fn new(watch: bool) -> Self {
		Self {
			watching: watch,
			debouncer: VfsDebouncer::new(),
			watched_paths: Vec::new(),
		}
	}
}

impl VfsBackend for StdBackend {
	fn read(&self, path: &Path) -> Result<Vec<u8>> {
		fs::read(path)
	}

	fn read_to_string(&self, path: &Path) -> Result<String> {
		let contents = fs::read_to_string(path)?;

		if Config::new().ignore_line_endings && contents.contains('\r') {
			return Ok(contents.replace("\r\n", "\n").replace("\r", "\n"));
		}

		Ok(contents)
	}

	fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
		let mut paths = Vec::new();

		for entry in fs::read_dir(path)? {
			paths.push(entry?.path());
		}

		Ok(paths)
	}

	fn write(&mut self, path: &Path, contents: &[u8]) -> Result<()> {
		if let Ok(existing) = fs::read(path) {
			if existing == contents {
				return Ok(());
			}

			if Config::new().ignore_line_endings {
				let normalize = |value: &str| value.replace("\r\n", "\n").replace('\r', "\n");
				if let (Ok(existing), Ok(incoming)) = (std::str::from_utf8(&existing), std::str::from_utf8(contents)) {
					if normalize(existing) == normalize(incoming) {
						return Ok(());
					}
				}
			}
		}

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}

		let result = atomic_write(path, contents);
		if result.is_ok() {
			self.debouncer.record_syncback_path(path);
		}
		result
	}

	fn create_dir(&mut self, path: &Path) -> Result<()> {
		let result = fs::create_dir_all(path);
		if result.is_ok() {
			self.debouncer.record_syncback_path(path);
		}
		result
	}

	fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
		// Treat a repeated notification for an already completed rename as
		// success. This is common on Windows and avoids poisoning the rest of a
		// Studio batch with an os-error-3 from a stale source path.
		if from == to || (!from.exists() && to.exists()) {
			self.debouncer.record_syncback_path(from);
			self.debouncer.record_syncback_path(to);
			return Ok(());
		}

		if let Some(parent) = to.parent() {
			fs::create_dir_all(parent)?;
		}

		let result = fs::rename(from, to);
		if result.is_ok() {
			self.debouncer.record_syncback_path(from);
			self.debouncer.record_syncback_path(to);
		}
		result
	}

	fn remove(&mut self, path: &Path) -> Result<()> {
		self.unwatch(path)?;

		let result = if Config::new().move_to_bin {
			trash::delete(path).map_err(Error::other)
		} else if path.is_dir() {
			fs::remove_dir_all(path)
		} else {
			fs::remove_file(path)
		};

		if result.is_ok() {
			self.debouncer.record_syncback_path(path);
		}

		result
	}

	fn exists(&self, path: &Path) -> bool {
		path.exists()
	}

	fn is_dir(&self, path: &Path) -> bool {
		path.is_dir()
	}

	fn is_file(&self, path: &Path) -> bool {
		path.is_file()
	}

	fn path_kind(&self, path: &Path) -> VfsPathKind {
		match fs::metadata(path) {
			Ok(metadata) if metadata.is_file() => VfsPathKind::File,
			Ok(metadata) if metadata.is_dir() => VfsPathKind::Directory,
			_ => VfsPathKind::Missing,
		}
	}

	fn watch(&mut self, path: &Path, recursive: bool) -> Result<()> {
		let path = path.to_owned();

		if !self.watching || self.watched_paths.iter().any(|p| path.starts_with(p)) {
			return Ok(());
		}

		self.debouncer.watch(&path, recursive)?;
		self.watched_paths.push(path);

		Ok(())
	}

	fn unwatch(&mut self, path: &Path) -> Result<()> {
		if !self.watching {
			return Ok(());
		}

		let path = path.to_owned();

		self.watched_paths.retain(|p| {
			let unwatch = p.starts_with(&path);

			if unwatch {
				self.debouncer.unwatch(p).ok();
			}

			!unwatch
		});

		Ok(())
	}

	fn pause(&mut self) {
		self.debouncer.pause()
	}

	fn resume(&mut self) {
		self.debouncer.resume()
	}

	fn receiver(&self) -> Receiver<VfsEvent> {
		self.debouncer.receiver()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use uuid::Uuid;

	#[test]
	fn write_creates_missing_parent_directories() {
		let root = std::env::temp_dir().join(format!("argon-vfs-{}", Uuid::new_v4()));
		let path = root.join("missing").join("nested").join("instance.meta.json");
		let mut backend = StdBackend::new(false);

		backend.write(&path, b"{}").unwrap();

		assert_eq!(fs::read(&path).unwrap(), b"{}");
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn atomic_temp_name_is_shorter_than_instance_metadata_name() {
		let target = Path::new("deep").join("init.meta.json");
		let token = Uuid::new_v4().simple().to_string();
		let temp = target.with_file_name(format!(".a{}", &token[..8]));

		assert!(temp.as_os_str().len() < target.as_os_str().len());
	}

	#[test]
	fn write_preserves_existing_line_endings_when_text_is_equivalent() {
		let root = std::env::temp_dir().join(format!("argon-vfs-line-endings-{}", Uuid::new_v4()));
		let path = root.join("Script.luau");
		fs::create_dir_all(&root).unwrap();
		fs::write(&path, b"local x = 1\r\nreturn x\r\n").unwrap();

		let mut backend = StdBackend::new(false);
		backend.write(&path, b"local x = 1\nreturn x\n").unwrap();

		assert_eq!(fs::read(&path).unwrap(), b"local x = 1\r\nreturn x\r\n");
		fs::remove_dir_all(root).unwrap();
	}
}
