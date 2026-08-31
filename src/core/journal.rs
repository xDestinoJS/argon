use anyhow::{Context, Result};
use std::{
	fs::{self, File, OpenOptions},
	io::{Read, Write},
	path::{Path, PathBuf},
	sync::Mutex,
	time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::core::changes::Changes;

pub struct Journal {
	dir: PathBuf,
	legacy_path: PathBuf,
	sequence: Mutex<u128>,
}

impl Journal {
	pub fn new(project_path: &Path) -> Self {
		let workspace = project_path.parent().unwrap_or(project_path);
		let canonical = project_path.canonicalize().unwrap_or_else(|_| project_path.to_owned());
		let mut project_hash = 0xcbf29ce484222325u64;
		for byte in canonical.to_string_lossy().as_bytes() {
			project_hash ^= u64::from(*byte);
			project_hash = project_hash.wrapping_mul(0x100000001b3);
		}
		let dir = crate::util::get_argon_dir()
			.unwrap_or_else(|_| workspace.join(".argon"))
			.join("journals")
			.join(format!("{project_hash:016x}"));
		let _ = fs::create_dir_all(&dir);

		Self {
			dir,
			legacy_path: workspace.join(".argon").join("journal.bin"),
			sequence: Mutex::new(0),
		}
	}

	/// Persist one complete client batch before it enters the in-memory processor
	/// queue. Each batch owns a file so completing an earlier write can never
	/// erase a later batch which is still waiting to be applied.
	pub fn append(&self, changes: &Changes) -> Result<Option<PathBuf>> {
		if changes.is_empty() {
			return Ok(None);
		}

		fs::create_dir_all(&self.dir)?;
		let bytes = rmp_serde::to_vec(changes).context("Failed to serialize crash journal batch")?;
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_nanos();
		let mut last_sequence = self.sequence.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let sequence = now.max(last_sequence.saturating_add(1));
		*last_sequence = sequence;
		let name = format!("{sequence:039}-{}", Uuid::new_v4().simple());
		let path = self.dir.join(format!("{name}.bin"));
		let temp_path = self.dir.join(format!(".{name}.tmp"));

		let result = (|| -> Result<()> {
			let mut file = OpenOptions::new().create_new(true).write(true).open(&temp_path)?;
			file.write_all(&bytes)?;
			// flush() only reaches the operating-system cache. sync_all() is the
			// durability boundary required before acknowledging the request.
			file.sync_all()?;
			drop(file);
			fs::rename(&temp_path, &path)?;
			sync_directory(&self.dir)?;
			Ok(())
		})();

		if result.is_err() {
			let _ = fs::remove_file(&temp_path);
		}
		result?;

		Ok(Some(path))
	}

	pub fn recover(&self) -> Vec<(PathBuf, Changes)> {
		let mut result = Vec::new();

		if let Ok(entries) = fs::read_dir(&self.dir) {
			let mut paths = entries
				.filter_map(|entry| entry.ok().map(|entry| entry.path()))
				.filter(|path| path.extension().is_some_and(|extension| extension == "bin"))
				.collect::<Vec<_>>();
			paths.sort();

			for path in paths {
				if let Ok(bytes) = fs::read(&path) {
					if let Ok(changes) = rmp_serde::from_slice::<Changes>(&bytes) {
						result.push((path, changes));
					}
				}
			}
		}

		// Recover journals created by versions before per-batch entries. This is
		// intentionally read last; it is only a backwards-compatibility path.
		if let Ok(mut file) = File::open(&self.legacy_path) {
			let mut len_buf = [0u8; 4];
			while file.read_exact(&mut len_buf).is_ok() {
				let len = u32::from_le_bytes(len_buf) as usize;
				let mut data_buf = vec![0u8; len];
				if file.read_exact(&mut data_buf).is_err() {
					break;
				}
				if let Ok(changes) = rmp_serde::from_slice::<Changes>(&data_buf) {
					result.push((self.legacy_path.clone(), changes));
				}
			}
		}

		result
	}

	pub fn complete(&self, entry: Option<&Path>) {
		if let Some(entry) = entry {
			let _ = fs::remove_file(entry);
			let _ = sync_directory(entry.parent().unwrap_or(&self.dir));
		}
	}
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
	File::open(path)?.sync_all()?;
	Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
	// The journal file itself has already been flushed with sync_all. Windows
	// does not expose directory handles through std::fs::File.
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn durable_batches_are_independent_and_recoverable() {
		let workspace = std::env::temp_dir().join(format!("argon-journal-{}", Uuid::new_v4()));
		fs::create_dir_all(&workspace).unwrap();
		let project_path = workspace.join("default.project.json");
		fs::write(&project_path, b"{}").unwrap();
		let journal = Journal::new(&project_path);

		let mut first = Changes::new();
		first.remove(rbx_dom_weak::types::Ref::new());
		let mut second = Changes::new();
		second.remove(rbx_dom_weak::types::Ref::new());

		let first_entry = journal.append(&first).unwrap().unwrap();
		let second_entry = journal.append(&second).unwrap().unwrap();
		assert_eq!(journal.recover().len(), 2);

		journal.complete(Some(&first_entry));
		let recovered = journal.recover();
		assert_eq!(recovered.len(), 1);
		assert_eq!(recovered[0].0, second_entry);

		journal.complete(Some(&second_entry));
		let _ = fs::remove_dir_all(&journal.dir);
		fs::remove_dir_all(workspace).unwrap();
	}
}
