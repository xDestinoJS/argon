use anyhow::{anyhow, Result};
use clap::Parser;
use colored::Colorize;
use std::{
	path::PathBuf,
	process::Command,
};

use crate::{
	argon_info,
	core::Core,
	ext::PathExt,
	project::{self, Project},
};

/// Convert between .meta.json folder trees and .rbxmx XML model files
#[derive(Parser)]
pub struct Convert {
	/// Input path to convert (folder or model file)
	#[arg()]
	path: Option<PathBuf>,

	/// Output path
	#[arg(short, long)]
	output: Option<PathBuf>,

	/// Convert to XML format (.rbxmx)
	#[arg(short, long)]
	xml: bool,
}

impl Convert {
	pub fn main(self) -> Result<()> {
		// 1. Perform Git backup commit first if git working tree has changes
		let status = Command::new("git")
			.args(["status", "--porcelain"])
			.output();

		if let Ok(output) = status {
			if !output.stdout.is_empty() {
				argon_info!("Uncommitted changes detected. Creating Git backup commit before conversion..");
				let _ = Command::new("git").args(["add", "-A"]).status();
				let _ = Command::new("git")
					.args(["commit", "-m", "pre-convert: backup meta files before rbxmx conversion"])
					.status();
			}
		}

		let path = self.path.unwrap_or_else(|| PathBuf::from("."));

		if !path.exists() {
			return Err(anyhow!("Path does not exist: {}", path.display()));
		}

		argon_info!("Converting {}..", path.display().to_string().bold());

		let project_path = project::resolve(path.clone())?;
		let project = Project::load(&project_path)?;

		let core = Core::new(project, false)?;

		let output_path = if let Some(output) = self.output {
			output
		} else if path.is_dir() {
			if path == PathBuf::from(".") || path.file_name().is_none() {
				let parent = project_path.get_parent();
				parent.join(format!("{}.rbxmx", core.project().name))
			} else {
				let name = path.file_name().unwrap().to_string_lossy();
				let parent = path.get_parent();
				parent.join(format!("{}.rbxmx", name))
			}
		} else {
			path.with_extension("json")
		};

		core.build(&output_path, true)?;

		argon_info!("Successfully converted to {}!", output_path.display().to_string().bold());

		Ok(())
	}
}
