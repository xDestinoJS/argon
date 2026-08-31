use anyhow::{Context as AnyhowContext, Result};
use log::{error, trace, warn};
use path_clean::PathClean;
use rbx_dom_weak::{types::Ref, ustr, Instance, Ustr};
use std::path::{Path, PathBuf};

use crate::{
	config::Config,
	core::{
		helpers::syncback::{rename_path, serialize_properties, validate_properties, verify_name, verify_path},
		meta::{Meta, NodePath, Source, SourceEntry, SourceKind, SyncbackFilter},
		snapshot::{AddedSnapshot, Snapshot, UpdatedSnapshot},
		tree::Tree,
	},
	ext::PathExt,
	middleware::{
		data::{self, write_original_name},
		dir, Middleware,
	},
	project::{Project, ProjectNode},
	vfs::Vfs,
	Properties,
};

macro_rules! filter_warn {
	($id:expr) => {
		warn!("Instance {} does not pass syncback filter! Skipping..", $id);
	};
	($id:expr, $path:expr) => {
		warn!(
			"Path: {} (source of instance: {}) does not pass syncback filter! Skipping..",
			$path.display(),
			$id
		);
	};
}

fn preserve_existing_properties_for_identity_update(properties: &mut Properties, existing: &Properties) {
	if properties.len() != 1 || !properties.contains_key(&ustr("Attributes")) {
		return;
	}

	for (property, value) in existing {
		if *property != ustr("Attributes") {
			properties.entry(*property).or_insert_with(|| value.clone());
		}
	}
}

fn expand_partial_properties(snapshot: &mut UpdatedSnapshot, existing: &Properties) {
	if !snapshot.partial_properties {
		return;
	}

	let Some(patch) = snapshot.properties.take() else {
		return;
	};

	let mut properties = existing.clone();
	if patch.contains_key(&ustr("CFrame")) {
		properties.remove(&ustr("Position"));
		properties.remove(&ustr("Orientation"));
		properties.remove(&ustr("Rotation"));
	}
	properties.extend(patch);
	snapshot.properties = Some(properties);
}

fn prune_filtered_children(snapshot: &mut Snapshot, filter: &SyncbackFilter) {
	snapshot.children.retain_mut(|child| {
		if filter.matches_name(&child.name) || filter.matches_class(&child.class) {
			return false;
		}

		prune_filtered_children(child, filter);
		true
	});
}

pub fn apply_addition(snapshot: AddedSnapshot, tree: &mut Tree, vfs: &Vfs) -> Result<()> {
	trace!("Adding {:?} with parent {:?}", snapshot.id, snapshot.parent);

	if !tree.exists(snapshot.parent) {
		warn!(
			"Attempted to add instance: {:?} whose parent doesn't exist: {:?}",
			snapshot.id, snapshot.parent
		);
		return Ok(());
	}

	let parent_id = snapshot.parent;
	let mut snapshot = Snapshot::from(snapshot);
	let Some(mut parent_meta) = tree.get_meta(parent_id).cloned() else {
		warn!(
			"Attempted to add instance {:?} whose parent meta doesn't exist: {:?}",
			snapshot.id, parent_id
		);
		return Ok(());
	};
	let filter = parent_meta.context.syncback_filter();

	if filter.matches_name(&snapshot.name) || filter.matches_class(&snapshot.class) {
		filter_warn!(snapshot.id);
		return Ok(());
	}

	snapshot.properties = validate_properties(snapshot.properties, filter);
	prune_filtered_children(&mut snapshot, filter);

	fn locate_instance_data(is_dir: bool, path: &Path, snapshot: &Snapshot, parent_meta: &Meta) -> Result<PathBuf> {
		parent_meta
			.context
			.sync_rules_of_type(&Middleware::InstanceData, true)
			.iter()
			.find_map(|rule| rule.locate(path, &snapshot.name, is_dir))
			.with_context(|| format!("Failed to locate data path for parent: {}", path.display()))
	}

	fn write_instance(
		has_children: bool,
		path: &mut PathBuf,
		snapshot: &mut Snapshot,
		parent_meta: &Meta,
		vfs: &Vfs,
	) -> Result<Option<Meta>> {
		let mut meta = snapshot.meta.clone().with_context(&parent_meta.context);
		let filter = parent_meta.context.syncback_filter();
		let mut properties = snapshot.properties.clone();

		if let Some(middleware) = Middleware::from_class(
			&snapshot.class,
			if !parent_meta.context.use_legacy_scripts() {
				Some(&mut properties)
			} else {
				None
			},
		) {
			let mut file_path = parent_meta
				.context
				.sync_rules_of_type(&middleware, true)
				.iter()
				.find_map(|rule| rule.locate(path, &snapshot.name, has_children))
				.with_context(|| format!("Failed to locate file path for parent: {}", path.display()))?;

			if has_children {
				if filter.matches_path(path) {
					filter_warn!(snapshot.id, path);
					return Ok(None);
				}

				if !verify_path(path, &mut snapshot.name, &mut meta, snapshot.id, vfs) {
					return Ok(None);
				}

				dir::write_dir(path, vfs)?;

				meta.set_source(Source::child_file(path, &file_path));
			} else {
				if !verify_path(&mut file_path, &mut snapshot.name, &mut meta, snapshot.id, vfs) {
					return Ok(None);
				}

				meta.set_source(Source::file(&file_path));
			}

			if filter.matches_path(&file_path) {
				filter_warn!(snapshot.id, &file_path);
				return Ok(None);
			}

			let properties = middleware.write(properties, &file_path, vfs)?;
			let data_path = locate_instance_data(has_children, path, snapshot, parent_meta)?;

			if filter.matches_path(&data_path) {
				filter_warn!(snapshot.id, &data_path);
			} else {
				let data_path = data::write_data(true, &snapshot.class, properties, &data_path, &meta, vfs)?;
				meta.source.set_data(data_path);
			}
		} else {
			if filter.matches_path(path) {
				filter_warn!(snapshot.id, path);
				return Ok(None);
			}

			if !verify_path(path, &mut snapshot.name, &mut meta, snapshot.id, vfs) {
				return Ok(None);
			}

			dir::write_dir(path, vfs)?;

			meta.set_source(Source::directory(path));

			let data_path = locate_instance_data(true, path, snapshot, parent_meta)?;

			if filter.matches_path(&data_path) {
				filter_warn!(snapshot.id, &data_path);
			} else {
				let data_path = data::write_data(false, &snapshot.class, properties, &data_path, &meta, vfs)?;
				meta.source.set_data(data_path);
			}
		}

		Ok(Some(meta))
	}

	fn add_non_project_instances(
		parent_id: Ref,
		parent_path: &Path,
		mut snapshot: Snapshot,
		parent_meta: &mut Meta,
		tree: &mut Tree,
		vfs: &Vfs,
	) -> Result<Source> {
		let config = Config::new();

		let mut parent_path = parent_path.to_owned();

		// Transform parent instance source from file to folder
		let parent_source = if vfs.is_file(&parent_path) {
			let sync_rule = parent_meta
				.context
				.sync_rules()
				.iter()
				.filter(|rule| {
					if let Some(pattern) = rule.child_pattern.as_ref() {
						!((pattern.as_str().starts_with(".src") || pattern.as_str().ends_with(".data.json"))
							&& config.rojo_mode)
					} else {
						true
					}
				})
				.find(|rule| rule.matches(&parent_path))
				.with_context(|| format!("Failed to find sync rule for path: {}", parent_path.display()))?
				.clone();

			let name = sync_rule.get_name(&parent_path);
			let mut folder_path = parent_path.with_file_name(&name);

			if !verify_path(&mut folder_path, &mut snapshot.name, parent_meta, parent_id, vfs) {
				return Ok(parent_meta.source.clone());
			}

			let file_path = sync_rule
				.locate(&folder_path, &name, true)
				.with_context(|| format!("Failed to locate file path for parent: {}", folder_path.display()))?;

			let data_paths = if let Some(data) = parent_meta.source.get_data() {
				let new_path = parent_meta
					.context
					.sync_rules_of_type(&Middleware::InstanceData, true)
					.iter()
					.find_map(|rule| rule.locate(&folder_path, &name, true))
					.with_context(|| format!("Failed to locate data path for parent: {}", folder_path.display()))?;

				Some((data.path().to_owned(), new_path))
			} else {
				None
			};

			let mut source = Source::child_file(&folder_path, &file_path);

			dir::write_dir(&folder_path, vfs)?;
			vfs.rename(&parent_path, &file_path)?;

			if let Some(data_paths) = data_paths {
				source.add_data(&data_paths.1);
				vfs.rename(&data_paths.0, &data_paths.1)?;
			}

			parent_path = folder_path;

			source
		} else {
			parent_meta.source.clone()
		};

		if !verify_name(&mut snapshot.name, &mut snapshot.meta) {
			return Ok(parent_source);
		}

		let mut path = parent_path.join(&snapshot.name);

		if tree.exists(snapshot.id) {
			if let Some(old_meta) = tree.get_meta(snapshot.id).cloned() {
				for entry in old_meta.source.relevant() {
					if let SourceEntry::Project(_) = entry {
						continue;
					}
					let old_path = entry.path();
					if vfs.exists(old_path) && old_path != &path && !old_path.starts_with(&path) {
						let _ = vfs.remove(old_path);
					}
				}
			}
			tree.remove_instance(snapshot.id);
		}

		if snapshot.children.is_empty() {
			if let Some(meta) = write_instance(false, &mut path, &mut snapshot, parent_meta, vfs)? {
				let snapshot = snapshot.with_meta(meta);

				tree.insert_instance_with_ref(snapshot, parent_id);
			}
		} else if let Some(mut meta) = write_instance(true, &mut path, &mut snapshot, parent_meta, vfs)? {
			let snapshot = snapshot.with_meta(meta.clone());

			tree.insert_instance_with_ref(snapshot.clone(), parent_id);

			for mut child in snapshot.children {
				child.properties = validate_properties(child.properties.clone(), meta.context.syncback_filter());
				add_non_project_instances(snapshot.id, &path, child, &mut meta, tree, vfs)?;
			}
		}

		Ok(parent_source)
	}

	fn add_project_instances(
		parent_id: Ref,
		path: &Path,
		node_path: NodePath,
		mut snapshot: Snapshot,
		parent_node: &mut ProjectNode,
		parent_meta: &Meta,
		tree: &mut Tree,
	) {
		let mut node = ProjectNode {
			class_name: Some(snapshot.class),
			properties: serialize_properties(&snapshot.class, snapshot.properties.clone()),
			..ProjectNode::default()
		};

		if snapshot.meta.keep_unknowns {
			node.keep_unknowns = Some(true);
		}

		let node_path = node_path.join(&snapshot.name);
		let source = Source::project(&snapshot.name, path, node.clone(), node_path.clone());
		let meta = snapshot
			.meta
			.clone()
			.with_context(&parent_meta.context)
			.with_source(source);

		snapshot.meta = meta;
		if tree.exists(snapshot.id) {
			tree.remove_instance(snapshot.id);
		}
		tree.insert_instance_with_ref(snapshot.clone(), parent_id);

		let filter = snapshot.meta.context.syncback_filter();

		for mut child in snapshot.children {
			child.properties = validate_properties(child.properties, filter);
			add_project_instances(parent_id, path, node_path.clone(), child, &mut node, parent_meta, tree);
		}

		parent_node.tree.insert(snapshot.name, node);
	}

	match parent_meta.source.get().clone() {
		SourceKind::Path(path) => {
			let parent_source = add_non_project_instances(parent_id, &path, snapshot, &mut parent_meta, tree, vfs)?;

			parent_meta.set_source(parent_source);
			tree.update_meta(parent_id, parent_meta);
		}
		SourceKind::Project(name, path, node, node_path) => {
			if let Some(custom_path) = &node.path {
				let custom_path = path.with_file_name(custom_path.path()).clean();

				let parent_source =
					add_non_project_instances(parent_id, &custom_path, snapshot, &mut parent_meta, tree, vfs)?;

				let parent_source = Source::project(&name, &path, *node, node_path.clone())
					.with_relevant(parent_source.relevant().to_owned());

				parent_meta.set_source(parent_source);
				tree.update_meta(parent_id, parent_meta);
			} else {
				let mut project = Project::load(&path)?;

				let node = project
					.find_node_by_path(&node_path)
					.context(format!("Failed to find project node with path {node_path:?}"))?;

				add_project_instances(parent_id, &path, node_path.clone(), snapshot, node, &parent_meta, tree);

				project.save(&path)?;
			}
		}
		SourceKind::None => panic!(
			"Attempted to add instance whose parent has no source: {:?}",
			snapshot.id
		),
	}

	Ok(())
}

pub fn apply_update(snapshot: UpdatedSnapshot, tree: &mut Tree, vfs: &Vfs) -> Result<()> {
	trace!("Updating {:?}", snapshot.id);

	if let Some(instance) = tree.get_instance(snapshot.id) {
		let Some(meta_ref) = tree.get_meta(snapshot.id) else {
			return Ok(());
		};
		let filter = meta_ref.context.syncback_filter();

		if filter.matches_name(&instance.name) || filter.matches_class(&instance.class) {
			filter_warn!(snapshot.id);
			return Ok(());
		}

		if snapshot.name.as_ref().is_some_and(|name| filter.matches_name(name)) {
			filter_warn!(snapshot.id);
			return Ok(());
		}

		if snapshot.class.as_ref().is_some_and(|class| filter.matches_class(class)) {
			filter_warn!(snapshot.id);
			return Ok(());
		}
	} else {
		warn!("Attempted to update instance that doesn't exist: {:?}", snapshot.id);
		return Ok(());
	}

	let Some(meta_val) = tree.get_meta(snapshot.id) else {
		return Ok(());
	};
	let mut meta = meta_val.clone();

	let Some(instance) = tree.get_instance_mut(snapshot.id) else {
		return Ok(());
	};

	// Argon sends an attribute-only update while repairing/persisting an
	// instance identity. Updates normally replace the serialized property map
	// so resetting a property to its Roblox default removes stale disk state,
	// but an identity patch must never erase unrelated properties that are
	// already persisted for the instance.
	let mut snapshot = snapshot;
	expand_partial_properties(&mut snapshot, &instance.properties);
	if let Some(properties) = snapshot.properties.as_mut() {
		preserve_existing_properties_for_identity_update(properties, &instance.properties);
	}

	fn locate_instance_data(name: &str, path: &Path, meta: &Meta, vfs: &Vfs) -> Option<PathBuf> {
		let data_path = if let Some(data) = meta.source.get_data() {
			Some(data.path().to_owned())
		} else {
			meta.context
				.sync_rules_of_type(&Middleware::InstanceData, true)
				.iter()
				.find_map(|rule| rule.locate(path, name, vfs.is_dir(path)))
		};

		if data_path.is_none() {
			warn!("Failed to locate instance data for {}", path.display())
		}

		data_path
	}

	fn remove_empty_rename_leftover(path: &Path, vfs: &Vfs) {
		// A directory rename should move the directory itself. If Windows or a
		// previous partial sync leaves the old source behind, remove it only when
		// it is demonstrably empty; this prevents it being re-imported as Folder.
		if vfs.exists(path) && vfs.is_dir(path) && vfs.read_dir(path).is_ok_and(|entries| entries.is_empty()) {
			if let Err(err) = vfs.remove(path) {
				warn!("Failed to remove empty rename leftover {}: {err}", path.display());
			}
		}
	}

	fn update_non_project_properties(
		path: &Path,
		properties: Properties,
		instance: &mut Instance,
		meta: &mut Meta,
		vfs: &Vfs,
	) -> Result<()> {
		let filter = meta.context.syncback_filter();

		if filter.matches_path(path) {
			filter_warn!(instance.referent(), path);
			return Ok(());
		}

		let mut properties = validate_properties(properties, filter);

		if let Some(middleware) = Middleware::from_class(
			&instance.class,
			if !meta.context.use_legacy_scripts() {
				Some(&mut properties)
			} else {
				None
			},
		) {
			let new_path = {
				let mut paths = meta
					.context
					.sync_rules_of_type(&middleware, true)
					.iter()
					.filter_map(|rule| rule.locate(path, &instance.name, vfs.is_dir(path)))
					.collect::<Vec<PathBuf>>();

				paths.sort_by_key(|path| !path.exists());
				paths.first().map(|path| path.to_owned())
			};

			let file_path = if let Some(SourceEntry::File(path)) = meta.source.get_file_mut() {
				let mut current_path = path.to_owned();

				if let Some(new_path) = new_path {
					if current_path != new_path {
						vfs.rename(&current_path, &new_path)?;

						*path = new_path.clone();
						current_path = new_path;
					}
				}

				Some(current_path)
			} else {
				if let Some(new_path) = &new_path {
					meta.source.add_file(new_path);
				}

				new_path
			};

			if let Some(file_path) = file_path {
				let properties = middleware.write(properties.clone(), &file_path, vfs)?;

				if let Some(data_path) = locate_instance_data(&instance.name, path, meta, vfs) {
					if filter.matches_path(&data_path) {
						filter_warn!(instance.referent(), &data_path);
					} else {
						let data_path = data::write_data(true, &instance.class, properties, &data_path, meta, vfs)?;
						meta.source.set_data(data_path)
					}
				}
			} else {
				error!("Failed to locate file for path {:?}", path.display());
			}
		} else if let Some(data_path) = locate_instance_data(&instance.name, path, meta, vfs) {
			if filter.matches_path(&data_path) {
				filter_warn!(instance.referent(), &data_path);
			} else {
				let data_path = data::write_data(false, &instance.class, properties.clone(), &data_path, meta, vfs)?;
				meta.source.set_data(data_path)
			}
		}

		instance.properties = properties;

		Ok(())
	}

	match meta.source.get().clone() {
		SourceKind::Path(mut path) => {
			if let Some(mut name) = snapshot.name {
				let original_name = meta.original_name.clone();

				if !verify_name(&mut name, &mut meta) {
					return Ok(());
				}

				path = rename_path(&path, &instance.name, &name, vfs);

				if !verify_path(&mut path, &mut name, &mut meta, snapshot.id, vfs) {
					return Ok(());
				}

				*meta.source.get_mut() = SourceKind::Path(path.clone());

				let filter = meta.context.syncback_filter();

				if let Some(SourceEntry::Folder(folder_path)) = meta.source.get_folder_mut() {
					let new_path = rename_path(folder_path, &instance.name, &name, vfs);

					if filter.matches_path(folder_path) && filter.matches_path(&new_path) {
						filter_warn!(snapshot.id, folder_path);
					} else {
						let old_path = folder_path.clone();
						*folder_path = new_path.clone();

						for mut entry in meta.source.relevant_mut() {
							match &mut entry {
								SourceEntry::File(path) | SourceEntry::Data(path) => {
									*path = new_path.join(path.get_name());
								}
								_ => continue,
							}
						}

						vfs.rename(&old_path, &new_path)?;
						remove_empty_rename_leftover(&old_path, vfs);
					}
				} else {
					for mut entry in meta.source.relevant_mut() {
						match &mut entry {
							SourceEntry::File(path) | SourceEntry::Data(path) => {
								let new_path = rename_path(path, &instance.name, &name, vfs);

								if filter.matches_path(path) && filter.matches_path(&new_path) {
									filter_warn!(snapshot.id, path);
									continue;
								}

								let old_path = path.clone();
								*path = new_path.clone();
								vfs.rename(&old_path, &new_path)?;
							}
							_ => continue,
						}
					}
				}

				if original_name != meta.original_name && snapshot.properties.is_none() {
					if let Some(data_path) = locate_instance_data(&name, &path, &meta, vfs) {
						if filter.matches_path(&data_path) {
							filter_warn!(instance.referent(), &data_path);
						} else {
							write_original_name(&data_path, &meta, vfs)?;
						}
					}
				}

				instance.name = meta.original_name.clone().unwrap_or(name);
			}

			if let Some(properties) = snapshot.properties {
				update_non_project_properties(&path, properties, instance, &mut meta, vfs)?;
			}

			tree.update_meta(snapshot.id, meta);

			if let Some(_class) = snapshot.class {
				// You can't change the class of an instance inside Roblox Studio
				unreachable!()
			}

			if let Some(_meta) = snapshot.meta {
				// Currently Argon client does not update meta
				unreachable!()
			}
		}
		SourceKind::Project(name, path, node, node_path) => {
			let mut project = Project::load(&path)?;
			let mut project_changed = false;

			if let Some(properties) = snapshot.properties {
				if node.path.is_some() {
					// A `$path` node describes a filesystem mount, not a Studio-created
					// instance. Never manufacture an init.meta.json beside that mount.
					// Keep the validated state in memory; explicit project properties
					// remain owned by default.project.json.
					instance.properties = validate_properties(properties, meta.context.syncback_filter());
				} else {
					let node = project
						.find_node_by_path(&node_path)
						.context(format!("Failed to find project node with path {node_path:?}"))?;

					let class = node.class_name.unwrap_or(Ustr::from(&name));
					let properties = validate_properties(properties, meta.context.syncback_filter());

					node.properties = serialize_properties(&class, properties.clone());
					node.tags = Vec::new();
					node.keep_unknowns = None;

					instance.properties = properties;
					project_changed = true;
				}
			}

			// It has to be done after updating properties as it may change the node path
			if let Some(new_name) = snapshot.name {
				let parent_node = project.find_node_by_path(&node_path.parent()).with_context(|| {
					format!("Failed to find parent project node with path {:?}", node_path.parent())
				})?;

				let node = parent_node
					.tree
					.remove(&name)
					.context(format!("Failed to remove project node with path {node_path:?}"))?;

				parent_node.tree.insert(new_name.clone(), node.clone());

				let node_path = node_path.parent().join(&new_name);

				*meta.source.get_mut() = SourceKind::Project(new_name.clone(), path.clone(), Box::new(node), node_path);

				instance.name = new_name;
				project_changed = true;
			}

			tree.update_meta(snapshot.id, meta);
			if project_changed {
				project.save(&path)?;
			}

			if let Some(_class) = snapshot.class {
				// You can't change the class of an instance inside Roblox Studio
				unreachable!()
			}

			if let Some(_meta) = snapshot.meta {
				// Currently Argon client does not update meta
				unreachable!()
			}
		}
		SourceKind::None => panic!("Attempted to update instance with no source: {:?}", snapshot.id),
	}

	Ok(())
}

pub fn apply_removal(id: Ref, tree: &mut Tree, vfs: &Vfs) -> Result<()> {
	trace!("Removing {id:?}");

	if let Some(instance) = tree.get_instance(id) {
		let Some(meta_ref) = tree.get_meta(id) else {
			return Ok(());
		};
		let filter = meta_ref.context.syncback_filter();

		if filter.matches_name(&instance.name) || filter.matches_class(&instance.class) {
			filter_warn!(id);
			return Ok(());
		}
	} else {
		warn!("Attempted to remove instance that doesn't exist: {id:?}");
		return Ok(());
	}

	let Some(meta_val) = tree.get_meta(id) else {
		return Ok(());
	};
	let meta = meta_val.clone();

	fn remove_non_project_instances(id: Ref, meta: &Meta, _tree: &mut Tree, vfs: &Vfs) -> Result<()> {
		let filter = meta.context.syncback_filter();

		for entry in meta.source.relevant() {
			match entry {
				SourceEntry::Project(_) => continue,
				_ => {
					let path = entry.path();

					if vfs.exists(path) {
						if filter.matches_path(path) {
							filter_warn!(id, path);
						} else {
							vfs.remove(path)?;
							if let Some(parent) = path.parent() {
								if vfs.exists(parent) && vfs.is_dir(parent) {
									if let Ok(entries) = vfs.read_dir(parent) {
										if entries.is_empty() {
											let _ = vfs.remove(parent);
										}
									}
								}
							}
						}
					}
				}
			}
		}

		Ok(())
	}

	match meta.source.get() {
		SourceKind::Path(_) => remove_non_project_instances(id, &meta, tree, vfs)?,
		SourceKind::Project(name, path, node, node_path) => {
			let mut project = Project::load(path)?;
			let parent_node = project.find_node_by_path(&node_path.parent());

			parent_node.and_then(|node| node.tree.remove(name));

			if node.path.is_some() {
				remove_non_project_instances(id, &meta, tree, vfs)?;
			}

			project.save(path)?;
		}
		SourceKind::None => error!("Attempted to remove instance with no source: {id:?}"),
	}

	tree.remove_instance(id);

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::{expand_partial_properties, preserve_existing_properties_for_identity_update, prune_filtered_children};
	use crate::core::{
		meta::SyncbackFilter,
		snapshot::{Snapshot, UpdatedSnapshot},
	};
	use crate::Properties;
	use rbx_dom_weak::{
		types::{Content, Variant},
		ustr,
	};

	#[test]
	fn identity_only_update_preserves_existing_properties() {
		let mut incoming = Properties::default();
		incoming.insert(ustr("Attributes"), Variant::Attributes(Default::default()));

		let mut existing = Properties::default();
		existing.insert(ustr("Texture"), Variant::Content(Content::from("rbxasset://smoke")));
		existing.insert(ustr("Rate"), Variant::Float32(20.0));

		preserve_existing_properties_for_identity_update(&mut incoming, &existing);

		assert!(incoming.contains_key(&ustr("Texture")));
		assert!(incoming.contains_key(&ustr("Rate")));
	}

	#[test]
	fn partial_property_update_keeps_untouched_properties() {
		let mut existing = Properties::default();
		existing.insert(ustr("Color"), Variant::String("green".into()));
		existing.insert(ustr("Transparency"), Variant::Float32(0.5));
		existing.insert(ustr("Position"), Variant::String("stale alias".into()));

		let mut patch = Properties::default();
		patch.insert(ustr("CFrame"), Variant::String("updated transform".into()));

		let mut snapshot = UpdatedSnapshot::new(rbx_dom_weak::types::Ref::new());
		snapshot.properties = Some(patch);
		snapshot.partial_properties = true;
		expand_partial_properties(&mut snapshot, &existing);

		let properties = snapshot.properties.unwrap();
		assert!(properties.contains_key(&ustr("Color")));
		assert!(properties.contains_key(&ustr("Transparency")));
		assert!(properties.contains_key(&ustr("CFrame")));
		assert!(!properties.contains_key(&ustr("Position")));
	}

	#[test]
	fn nested_engine_owned_instances_are_pruned_from_studio_additions() {
		let mut snapshot = Snapshot::new().with_class("Model").with_children(vec![Snapshot::new()
			.with_name("Handle")
			.with_class("Part")
			.with_children(vec![
				Snapshot::new()
					.with_name("TouchInterest")
					.with_class("TouchTransmitter"),
				Snapshot::new().with_name("Kept script").with_class("Script"),
			])]);

		prune_filtered_children(&mut snapshot, &SyncbackFilter::default());

		let handle = &snapshot.children[0];
		assert_eq!(handle.children.len(), 1);
		assert_eq!(handle.children[0].class, "Script");
	}
}
