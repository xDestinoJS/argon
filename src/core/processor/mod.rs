use anyhow::{Context, Result};
use colored::Colorize;
use crossbeam_channel::{select, Sender};
use log::{debug, error, info, trace, warn};
use serde::Deserialize;
use std::{
	path::PathBuf,
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
	#[serde(skip)]
	pub journal_entries: Vec<PathBuf>,
}

pub struct Processor {
	writer: Sender<WriteRequest>,
	journal: Arc<crate::core::journal::Journal>,
	submission_lock: Mutex<()>,
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
			for (entry, changes) in recovered {
				let mut recovered_successfully = true;
				for id in changes.removals {
					recovered_successfully &= write::apply_removal(id, &mut tree_lock, &vfs).is_ok();
				}
				for snapshot in changes.additions {
					recovered_successfully &= write::apply_addition(snapshot, &mut tree_lock, &vfs).is_ok();
				}
				for snapshot in changes.updates {
					recovered_successfully &= write::apply_update(snapshot, &mut tree_lock, &vfs).is_ok();
				}
				if recovered_successfully {
					journal.complete(Some(&entry));
				} else {
					warn!(
						"Crash journal batch could not be fully recovered; keeping {} for the next start",
						entry.display()
					);
					break;
				}
			}
		}

		let handler = Arc::new(Handler {
			queue,
			tree,
			vfs: vfs.clone(),
			project,
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
							let mut requests = vec![request?];
							requests.extend(client_receiver.try_iter());
							vfs.pause();
							if !handler.on_client_events(requests) {
								anyhow::bail!("Durable Studio write failed; restart Argon to replay the retained journal");
							}
							vfs.resume();
						}
					}
				}
			})
			.unwrap();

		Self {
			writer: sender,
			journal,
			submission_lock: Mutex::new(()),
		}
	}

	pub fn write(&self, mut request: WriteRequest) -> Result<()> {
		let _submission = self
			.submission_lock
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		if let Some(entry) = self.journal.append(&request.changes)? {
			request.journal_entries.push(entry);
		}
		self.writer
			.send(request)
			.map_err(|err| anyhow::anyhow!("Failed to queue durable Studio changes: {err}"))
	}
}

struct Handler {
	queue: Arc<Queue>,
	tree: Arc<Mutex<Tree>>,
	vfs: Arc<Vfs>,
	project: Arc<Mutex<Project>>,
	journal: Arc<crate::core::journal::Journal>,
}

impl Handler {
	fn on_client_events(&self, requests: Vec<WriteRequest>) -> bool {
		let mut pending: Option<WriteRequest> = None;

		for request in requests {
			if let Some(current) = pending.as_mut() {
				if current.client_id == request.client_id
					&& current.changes.is_update_only()
					&& request.changes.is_update_only()
				{
					current.changes.coalesce_updates(request.changes);
					current.journal_entries.extend(request.journal_entries);
					continue;
				}
			}

			if let Some(current) = pending.replace(request) {
				if !self.on_client_event(current) {
					return false;
				}
			}
		}

		if let Some(current) = pending {
			return self.on_client_event(current);
		}

		true
	}

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

			let result = self.queue.push(server::SyncChanges(changes), None);

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
	fn on_client_event(&self, request: WriteRequest) -> bool {
		profiling::start_frame!();

		let journal_entries = request.journal_entries;
		let changes = request.changes;
		let changes_for_other_clients = changes.clone();
		let client_id = request.client_id;

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

		let mut tree = lock!(self.tree);

		let result = || -> Result<()> {
			let mut failures = Vec::new();

			for id in changes.removals {
				if let Err(err) = write::apply_removal(id, &mut tree, &self.vfs)
					.with_context(|| format!("Failed to apply removal {id:?}"))
				{
					failures.push(format!("{err:#}"));
				}
			}

			for snapshot in changes.additions {
				let id = snapshot.id;
				if let Err(err) = write::apply_addition(snapshot, &mut tree, &self.vfs)
					.with_context(|| format!("Failed to apply addition {id:?}"))
				{
					failures.push(format!("{err:#}"));
				}
			}

			for snapshot in changes.updates {
				let id = snapshot.id;
				if let Err(err) = write::apply_update(snapshot, &mut tree, &self.vfs)
					.with_context(|| format!("Failed to apply update {id:?}"))
				{
					failures.push(format!("{err:#}"));
				}
			}

			if failures.is_empty() {
				Ok(())
			} else {
				anyhow::bail!(failures.join("; "))
			}
		}();

		let succeeded = result.is_ok();
		match result {
			Ok(()) => {
				trace!("Changes applied successfully");
				// Delete durable entries only after every filesystem operation in the
				// combined batch succeeds. Failed or interrupted work is replayed on
				// the next server start.
				for entry in &journal_entries {
					self.journal.complete(Some(entry));
				}

				// A Studio-originated change is already present in the originating
				// place. Deliver it directly to other clients instead of relying on
				// the filesystem watcher to echo it back to everyone.
				if !changes_for_other_clients.is_empty() {
					if let Err(err) = self
						.queue
						.push_except(server::SyncChanges(changes_for_other_clients), client_id)
					{
						warn!("Failed to sync Studio changes to other clients: {err}");
					}
				}
			}
			Err(err) => error!("Failed to apply changes: {err:#}"),
		}

		self.queue.push(server::SyncbackChanges(), Some(0)).ok();
		succeeded
	}
}
