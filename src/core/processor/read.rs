use std::path::Path;

use log::{error, trace};
use rbx_dom_weak::types::Ref;

use crate::{
	core::{
		changes::Changes,
		meta::SourceKind,
		snapshot::{Snapshot, UpdatedSnapshot},
		tree::persisted_argon_id,
		tree::Tree,
	},
	middleware::{new_snapshot, project::new_snapshot_node},
	stats, util,
	vfs::Vfs,
};

pub fn process_changes(id: Ref, tree: &mut Tree, vfs: &Vfs) -> Option<Changes> {
	trace!("Processing changes for instance: {id:?}");

	let mut changes = Changes::new();

	let meta = tree.get_meta(id)?.clone();
	let source = meta.source.get().clone();

	let process_path = |path: &Path| -> Option<Option<Snapshot>> {
		match new_snapshot(path, &meta.context, vfs) {
			Ok(snapshot) => Some(snapshot),
			Err(err) => {
				error!("Failed to process changes: {err}, source: {source:?}");
				None
			}
		}
	};

	let snapshot = match &source {
		SourceKind::Project(name, path, node, node_path) => {
			if node_path.is_root() {
				process_path(path)?
			} else {
				match new_snapshot_node(name, path, *node.clone(), node_path.clone(), &meta.context, vfs) {
					Ok(snapshot) => Some(snapshot),
					Err(err) => {
						error!("Failed to process changes: {err}, source: {source:?}");
						return Some(changes);
					}
				}
			}
		}
		SourceKind::Path(path) => {
			let res = process_path(path)?;
			if res.is_none() {
				if let Some(inst) = tree.get_instance(id) {
					let new_path = crate::core::helpers::syncback::rename_path(path, &inst.name, &inst.name, vfs);
					if vfs.exists(&new_path) {
						let mut updated_meta = meta.clone();
						*updated_meta.source.get_mut() = SourceKind::Path(new_path.clone());
						tree.update_meta(id, updated_meta);
						process_path(&new_path)?
					} else {
						None
					}
				} else {
					None
				}
			} else {
				res
			}
		}
		SourceKind::None => panic!(
			"Fatal processing error: `SourceKind::None` should not be present in the tree! Id: {id:?}, meta: {meta:#?}"
		),
	};

	// Handle additions, modifications and child removals
	if let Some(snapshot) = snapshot {
		process_child_changes(id, snapshot, &mut changes, tree);
	// Handle regular removals
	} else {
		let current_meta = tree.get_meta(id);
		let path_exists = current_meta
			.and_then(|m| m.source.get().path())
			.map_or(false, |p| vfs.exists(p));
		if !path_exists {
			tree.remove_instance(id);
			changes.remove(id);
		}
	}

	Some(changes)
}

fn restore_studio_name(snapshot: &mut Snapshot) {
	// UUID suffixes created by `keepDuplicates` distinguish paths on disk only.
	// Rescans must compare the persisted `originalName` with the live DOM name;
	// otherwise every directory notification becomes a false server rename.
	if let Some(original_name) = &snapshot.meta.original_name {
		snapshot.name.clone_from(original_name);
	}
}

fn process_child_changes(id: Ref, mut snapshot: Snapshot, changes: &mut Changes, tree: &mut Tree) {
	restore_studio_name(&mut snapshot);

	// Process instance changes
	let mut updated_snapshot = UpdatedSnapshot::new(id);

	if let Some(existing_meta) = tree.get_meta(id) {
		if snapshot.meta != *existing_meta {
			tree.update_meta(id, snapshot.meta.clone());
			updated_snapshot.meta = Some(snapshot.meta);
		}
	}

	let Some(instance) = tree.get_instance_mut(id) else {
		return;
	};

	updated_snapshot.name = if snapshot.name != instance.name {
		instance.name.clone_from(&snapshot.name);
		Some(snapshot.name)
	} else {
		None
	};

	updated_snapshot.class = if snapshot.class != instance.class {
		instance.class.clone_from(&snapshot.class);
		Some(snapshot.class)
	} else {
		None
	};

	updated_snapshot.properties = if snapshot.properties != instance.properties {
		instance.properties.clone_from(&snapshot.properties);
		Some(snapshot.properties)
	} else {
		None
	};

	if !updated_snapshot.is_empty() {
		// Track `lines_synced` stat
		if let Some(properties) = &updated_snapshot.properties {
			let loc = util::count_loc_from_properties(properties);
			stats::lines_synced(loc as u32);
		}

		changes.update(updated_snapshot);
	}

	let mut hydrated = vec![false; snapshot.children.len()];

	// Pair instances and find removed children
	#[allow(clippy::unnecessary_to_owned)]
	for child_id in instance.children().to_owned() {
		let Some(instance) = tree.get_instance(child_id) else {
			continue;
		};

		let persisted_index = snapshot.children.iter().enumerate().find_map(|(index, child)| {
			(!hydrated[index] && child.class == instance.class && persisted_argon_id(child) == Some(child_id))
				.then_some(index)
		});
		let fallback_index = persisted_index.or_else(|| {
			snapshot.children.iter().enumerate().find_map(|(index, child)| {
				if hydrated[index] {
					return None;
				}

				let child_orig_name = child.meta.original_name.as_deref().unwrap_or(&child.name);
				((child.name == instance.name || child_orig_name == instance.name) && child.class == instance.class)
					.then_some(index)
			})
		});
		if let Some(index) = fallback_index {
			hydrated[index] = true;
		}

		if let Some(index) = fallback_index {
			snapshot.children[index].set_id(child_id);
		} else {
			tree.remove_instance(child_id);
			changes.remove(child_id);
		}
	}

	// Process child changes and find new children
	for child in snapshot.children {
		if child.id.is_some() {
			process_child_changes(child.id, child, changes, tree);
		} else {
			let mut child = child;

			insert_children(&mut child, id, tree);

			// Track `lines_synced` stat
			let loc = util::count_loc_from_properties(&child.properties);
			stats::lines_synced(loc as u32);

			changes.add(child, id);
		}
	}
}

fn insert_children(snapshot: &mut Snapshot, parent: Ref, tree: &mut Tree) {
	let id = tree.insert_instance(snapshot.clone(), parent);

	snapshot.set_id(id);

	for child in snapshot.children.iter_mut() {
		insert_children(child, id, tree);
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{process_changes, restore_studio_name};
	use crate::{
		core::{meta::Context, snapshot::Snapshot, tree::Tree},
		middleware::new_snapshot,
		vfs::Vfs,
	};

	#[test]
	fn rescan_removes_child_when_delete_event_was_missed() {
		let vfs = Vfs::new_virtual();
		let project_path = Path::new("project");
		let child_path = project_path.join("Part");

		vfs.create_dir(project_path).unwrap();
		vfs.create_dir(&child_path).unwrap();

		let snapshot = new_snapshot(project_path, &Context::default(), &vfs).unwrap().unwrap();
		let mut tree = Tree::new(snapshot);
		let root_ref = tree.root_ref();

		assert_eq!(tree.root().children().len(), 1);

		// Simulate a filesystem watcher missing the deletion entirely: disk has
		// changed, but no VFS event was passed to process_changes.
		vfs.remove(&child_path).unwrap();

		let changes = process_changes(root_ref, &mut tree, &vfs).unwrap();

		assert_eq!(changes.removals.len(), 1);
		assert!(tree.root().children().is_empty());
	}

	#[test]
	fn rescan_uses_original_name_instead_of_duplicate_storage_suffix() {
		let mut snapshot = Snapshot::new();
		snapshot.name = "Part_4065d3db-c11f-43da-8bf0-d6418c19d4fe".to_owned();
		snapshot.meta = crate::core::meta::Meta::new().with_original_name("Part".to_owned());

		restore_studio_name(&mut snapshot);

		assert_eq!(snapshot.name, "Part");
	}
}
