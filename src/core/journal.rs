use anyhow::Result;
use rmp_serde;
use std::{
	fs::{File, OpenOptions},
	io::{Read, Write},
	path::{Path, PathBuf},
};

use crate::core::changes::Changes;

pub struct Journal {
	path: PathBuf,
}

impl Journal {
	pub fn new(project_path: &Path) -> Self {
		let argon_dir = project_path.join(".argon");
		let _ = std::fs::create_dir_all(&argon_dir);
		Self {
			path: argon_dir.join("journal.bin"),
		}
	}

	pub fn append(&self, changes: &Changes) -> Result<()> {
		if changes.is_empty() {
			return Ok(());
		}
		if let Ok(bytes) = rmp_serde::to_vec(changes) {
			let mut file = OpenOptions::new()
				.create(true)
				.append(true)
				.open(&self.path)?;
			let len = (bytes.len() as u32).to_le_bytes();
			file.write_all(&len)?;
			file.write_all(&bytes)?;
			file.flush()?;
		}
		Ok(())
	}

	pub fn recover(&self) -> Vec<Changes> {
		let mut result = Vec::new();
		if let Ok(mut file) = File::open(&self.path) {
			let mut len_buf = [0u8; 4];
			while file.read_exact(&mut len_buf).is_ok() {
				let len = u32::from_le_bytes(len_buf) as usize;
				let mut data_buf = vec![0u8; len];
				if file.read_exact(&mut data_buf).is_ok() {
					if let Ok(changes) = rmp_serde::from_slice::<Changes>(&data_buf) {
						result.push(changes);
					}
				} else {
					break;
				}
			}
		}
		result
	}

	pub fn clear(&self) {
		let _ = std::fs::remove_file(&self.path);
	}
}
