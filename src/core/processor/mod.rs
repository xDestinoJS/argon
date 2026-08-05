use anyhow::Result;
use colored::Colorize;
use crossbeam_channel::{select, Sender};
use log::{debug, error, info, trace, warn};
use serde::Deserialize;
use std::{
	sync::{Arc, Mutex},
	thread::Builder,
};

use super::{changes::Changes, queue::Queue, tree::Tree};
use crate::{
	argon_error, argon_info,
	config::Config,
	constants::BLACKLISTED_PATHS,
	lock,
	project::{Project, ProjectDetails},
	server, stats,
	vfs::{Vfs, VfsEvent},
};

pub mod read;
pub mod write;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
	pub changes: Changes,
	pub client_id: u32,
}

pub struct Processor {
	writer: Sender<WriteRequest>,
}

impl Processor {
	pub fn new(queue: Arc<Queue>, tree: Arc<Mutex<Tree>>, vfs: Arc<Vfs>, project: Arc<Mutex<Project>>) -> Self {
		let project_path = lock!(project).path.clone();
		let journal = Arc::new(crate::core::journal::Journal::new(&project_path));

		// Crash recovery: replay any pending journal entries from an unexpected crash
		let recovered = journal.recover();
		if !recovered.is_empty() {
			info!("Recovering {} change batches from crash journal..", recovered.len());
			let mut tree_lock = lock!(tree);
			for changes in recovered {
				for snapshot in changes.additions {
					let _ = write::apply_addition(snapshot, &mut tree_lock, &vfs);
				}
				for snapshot in changes.updates {
					let _ = write::apply_update(snapshot, &mut tree_lock, &vfs);
				}
				for id in changes.removals {
					let _ = write::apply_removal(id, &mut tree_lock, &vfs);
				}
			}
			journal.clear();
		}

		let handler = Arc::new(Handler {
			queue,
			tree,
			vfs: vfs.clone(),
			project,
			last_syncback: Arc::new(Mutex::new(None)),
			journal: journal.clone(),
		});

		let handler = handler.clone();
		let (sender, receiver) = crossbeam_channel::unbounded();

		Builder::new()
			.name("processor".into())
			.spawn(move || -> Result<()> {
				let vfs_receiver = vfs.receiver();
				let client_receiver = receiver;

				loop {
					select! {
						recv(vfs_receiver) -> event => {
							handler.on_vfs_event(event?);
						}
						recv(client_receiver) -> request => {
							vfs.pause();
							handler.on_client_event(request?);
							vfs.resume();
						}
					}
				}
			})
			.unwrap();

		Self { writer: sender }
	}

	pub fn write(&self, request: WriteRequest) {
		self.writer.send(request).unwrap();
	}
}

struct Handler {
	queue: Arc<Queue>,
	tree: Arc<Mutex<Tree>>,
	vfs: Arc<Vfs>,
	project: Arc<Mutex<Project>>,
	last_syncback: Arc<Mutex<Option<(u32, std::time::Instant)>>>,
	journal: Arc<crate::core::journal::Journal>,
}

impl Handler {
	#[profiling::function]
	fn on_vfs_event(&self, event: VfsEvent) {
		profiling::start_frame!();

		trace!("Received VFS event: {event:?}");

		let mut tree = lock!(self.tree);
		let path = event.path();

		let changes = {
			if BLACKLISTED_PATHS.iter().any(|blacklisted| path.ends_with(blacklisted)) {
				trace!("Processing of {path:?} aborted: blacklisted");
				return;
			}

			let ids = {
				let mut current_path = path;

				loop {
					if let Some(ids) = tree.get_ids(current_path) {
						break ids.to_owned();
					}

					match current_path.parent() {
						Some(parent) => current_path = parent,
						None => {
							trace!("No ID found for path {path:?}");
							return;
						}
					}
				}
			};

			let mut changes = Changes::new();

			for id in ids {
				if let Some(processed) = read::process_changes(id, &mut tree, &self.vfs) {
					changes.extend(processed);
				}
			}

			changes
		};

		if !changes.is_empty() {
			stats::files_synced(changes.total() as u32);

			let except_id = {
				let lock = lock!(self.last_syncback);
				if let Some((client_id, timestamp)) = *lock {
					if timestamp.elapsed() < std::time::Duration::from_millis(1500) {
						Some(client_id)
					} else {
						None
					}
				} else {
					None
				}
			};

			let result = if let Some(client_id) = except_id {
				self.queue.push_except(server::SyncChanges(changes), client_id)
			} else {
				self.queue.push(server::SyncChanges(changes), None)
			};

			match result {
				Ok(()) => trace!("Added changes to the queue"),
				Err(err) => {
					error!("Failed to add changes to the queue: {err}");
				}
			}
		} else {
			trace!("No changes detected when processing path: {path:?}");
		}

		let mut project = lock!(self.project);

		if project.path == path {
			if let VfsEvent::Write(_) = event {
				debug!("Project file was modified. Reloading project..");

				let old_details = ProjectDetails::from_project(&project, &tree);

				match project.reload() {
					Ok(project) => {
						info!("Project reloaded");

						let details = ProjectDetails::from_project(project, &tree);

						if details == old_details {
							return;
						}

						match self.queue.push(server::SyncDetails(details), None) {
							Ok(()) => trace!("Project details synced"),
							Err(err) => warn!("Failed to sync project details: {err}"),
						}
					}
					Err(err) => error!("Failed to reload project: {err}"),
				}
			} else if let VfsEvent::Delete(_) = event {
				argon_error!("Warning! Top level project file was deleted. This might cause unexpected behavior. Skipping processing of changes!");
			}
		}
	}

	#[profiling::function]
	fn on_client_event(&self, request: WriteRequest) {
		profiling::start_frame!();

		let changes = request.changes;
		let client_id = request.client_id;

		let _ = self.journal.append(&changes);

		*lock!(self.last_syncback) = Some((client_id, std::time::Instant::now()));

		trace!("Received client event: {:?} changes", changes.total());

		if changes.total() > Config::new().changes_threshold {
			argon_info!(
				"Applying {}, {} and {} ({} total changes)",
				format!("{} additions", changes.additions.len()).bold().green(),
				format!("{} updates", changes.updates.len()).bold().blue(),
				format!("{} removals", changes.removals.len()).bold().red(),
				changes.total()
			);
		}

		fn log_ref_properties(name: &str, properties: &crate::Properties, children: &[crate::core::snapshot::Snapshot]) {
			for (p, v) in properties {
				let p_str = p.as_str();
				if matches!(v, rbx_dom_weak::types::Variant::Ref(_))
					|| p_str.starts_with("Attachment")
					|| p_str == "PrimaryPart"
					|| p_str == "Part0"
					|| p_str == "Part1"
					|| p_str == "Adornee"
					|| p_str == "Weld"
				{
					println!("ADDED ARGON ID FOR {}, for property '{}' ref to {:?}", name, p, v);
				}
			}
			for child in children {
				log_ref_properties(&child.name, &child.properties, &child.children);
			}
		}

		for snapshot in &changes.additions {
			log_ref_properties(&snapshot.name, &snapshot.properties, &snapshot.children);
		}

		for snapshot in &changes.updates {
			if let Some(properties) = &snapshot.properties {
				for (prop, val) in properties {
					let p_str = prop.as_str();
					if matches!(val, rbx_dom_weak::types::Variant::Ref(_))
						|| p_str.starts_with("Attachment")
						|| p_str == "PrimaryPart"
						|| p_str == "Part0"
						|| p_str == "Part1"
						|| p_str == "Adornee"
						|| p_str == "Weld"
					{
						println!("ADDED ARGON ID FOR (Update {:?}), for property '{}' ref to {:?}", snapshot.id, prop, val);
					}
				}
			}
		}

		let mut tree = lock!(self.tree);

		let result = || -> Result<()> {
			for snapshot in changes.additions {
				write::apply_addition(snapshot, &mut tree, &self.vfs)?;
			}

			for snapshot in changes.updates {
				write::apply_update(snapshot, &mut tree, &self.vfs)?;
			}

			for id in changes.removals {
				write::apply_removal(id, &mut tree, &self.vfs)?;
			}

			Ok(())
		}();

		match result {
			Ok(()) => trace!("Changes applied successfully"),
			Err(err) => error!("Failed to apply changes: {err}"),
		}

		self.queue.push(server::SyncbackChanges(), Some(0)).ok();
	}
}
