use anyhow::{bail, Result};
use colored::Colorize;
use log::{error, warn};
use path_clean::PathClean;
use rbx_dom_weak::{types::Tags, ustr, HashMapExt, UstrMap};
use std::path::{Path, PathBuf};

use super::{data, new_snapshot};
use crate::{
	argon_warn,
	config::Config,
	core::{
		meta::{Context, Meta, NodePath, Source, SourceEntry},
		snapshot::Snapshot,
	},
	ext::PathExt,
	middleware::helpers,
	project::{Project, ProjectNode, ProjectPath},
	util,
	vfs::Vfs,
};

#[profiling::function]
pub fn read_project(path: &Path, vfs: &Vfs) -> Result<Snapshot> {
	let mut project: Project = Project::load(path)?;

	vfs.watch(path, false)?;

	let features_root = if Config::new().feature_based_development {
		inject_feature_routes(&mut project.node, &project.workspace_dir, vfs)?
	} else {
		None
	};
	data::cleanup_project_owned_metadata(&project)?;

	let meta = Meta::from_project(&project);
	let mut snapshot = new_snapshot_node(&project.name, path, project.node, NodePath::new(), &meta.context, vfs)?;

	snapshot.meta.source.add_project(path);
	if let Some(features_root) = features_root {
		snapshot
			.meta
			.source
			.extend_relevant(vec![SourceEntry::Folder(features_root)]);
	}

	Ok(snapshot)
}

const FEATURE_ROUTES: &[(&str, &[&str])] = &[
	(
		"client",
		&["StarterPlayer", "StarterPlayerScripts", "Client", "Features"],
	),
	("server", &["ServerScriptService", "Server", "Features"]),
	("shared", &["ReplicatedStorage", "Shared", "Features"]),
];

fn inject_feature_routes(node: &mut ProjectNode, workspace: &Path, vfs: &Vfs) -> Result<Option<PathBuf>> {
	let features_root = workspace.join("src").join("Features");
	if !vfs.is_dir(&features_root) {
		return Ok(None);
	}

	vfs.watch(&features_root, true)?;

	let mut features = vfs.read_dir(&features_root)?;
	features.sort();

	for feature_path in features {
		if !vfs.is_dir(&feature_path) {
			continue;
		}

		let Some(feature_name) = feature_path.file_name().and_then(|name| name.to_str()) else {
			continue;
		};
		if feature_name.starts_with('.') {
			continue;
		}

		for (role, destination) in FEATURE_ROUTES {
			let role_path = feature_path.join(role);
			if !vfs.is_dir(&role_path) {
				continue;
			}

			let mut destination_node = &mut *node;
			for segment in *destination {
				destination_node = destination_node.tree.entry((*segment).to_owned()).or_default();
			}

			if destination_node.tree.contains_key(feature_name) {
				warn!(
					"Feature route {}.{} already exists in the project; keeping the explicit project mapping",
					destination.join("."),
					feature_name
				);
				continue;
			}

			destination_node.tree.insert(
				feature_name.to_owned(),
				ProjectNode {
					path: Some(ProjectPath::Required(
						role_path.strip_prefix(workspace).unwrap_or(&role_path).to_owned(),
					)),
					..ProjectNode::default()
				},
			);
		}
	}

	Ok(Some(features_root))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn routes_feature_roles_into_their_runtime_containers() {
		let vfs = Vfs::new_virtual();
		let workspace = Path::new("workspace");
		vfs.create_dir(&workspace.join("src/Features/Inventory/client"))
			.unwrap();
		vfs.create_dir(&workspace.join("src/Features/Inventory/server"))
			.unwrap();
		vfs.create_dir(&workspace.join("src/Features/Inventory/shared"))
			.unwrap();

		let mut root = ProjectNode::default();
		let watched = inject_feature_routes(&mut root, workspace, &vfs).unwrap();
		assert_eq!(watched, Some(workspace.join("src/Features")));

		let expected = [
			(
				&["StarterPlayer", "StarterPlayerScripts", "Client", "Features"][..],
				"src/Features/Inventory/client",
			),
			(
				&["ServerScriptService", "Server", "Features"][..],
				"src/Features/Inventory/server",
			),
			(
				&["ReplicatedStorage", "Shared", "Features"][..],
				"src/Features/Inventory/shared",
			),
		];

		for (destination, expected_path) in expected {
			let mut node = &root;
			for segment in destination {
				node = node.tree.get(*segment).unwrap();
			}
			let feature = node.tree.get("Inventory").unwrap();
			assert_eq!(feature.path, Some(ProjectPath::Required(PathBuf::from(expected_path))));
		}
	}
}

#[profiling::function]
pub fn new_snapshot_node(
	name: &str,
	path: &Path,
	node: ProjectNode,
	node_path: NodePath,
	context: &Context,
	vfs: &Vfs,
) -> Result<Snapshot> {
	if node.class_name.is_some() && node.path.is_some() {
		bail!("Failed to load project: $className and $path cannot be set at the same time");
	}

	let class = if let Some(class_name) = &node.class_name {
		class_name.to_owned()
	} else if util::is_service(name) {
		name.to_owned()
	} else {
		String::from("Folder")
	};
	let project_owned = node.path.is_some();

	let properties = {
		let mut properties = UstrMap::new();

		for (property, value) in &node.properties {
			match value.clone().resolve(&class, property) {
				Ok(value) => {
					properties.insert(*property, value);
				}
				Err(err) => {
					error!(
						"Failed to parse property: {} at {}, JSON path: {}",
						err,
						path.display(),
						node_path
					);
				}
			}
		}

		if let Some(attributes) = &node.attributes {
			match attributes.clone().resolve(&class, "Attributes") {
				Ok(value) => {
					properties.insert(ustr("Attributes"), value);
				}
				Err(err) => {
					error!(
						"Failed to parse attributes: {} at {}, JSON path: {}",
						err,
						path.display(),
						node_path
					);
				}
			}
		}

		if !node.tags.is_empty() {
			properties.insert(ustr("Tags"), Tags::from(node.tags.clone()).into());
		}

		properties
	};

	let mut meta = Meta::new()
		.with_source(Source::project(name, path, node.clone(), node_path.clone()))
		.with_context(context)
		.with_keep_unknowns(node.keep_unknowns.unwrap_or_else(|| util::is_service(&class)))
		.with_project_owned(project_owned);

	if class == "MeshPart" {
		meta.set_mesh_source(helpers::save_mesh(&properties));
	}

	let mut snapshot = Snapshot::new()
		.with_name(name)
		.with_class(&class)
		.with_properties(properties)
		.with_meta(meta);

	if let Some(path_node) = node.path {
		let path = path.with_file_name(path_node.path()).clean();

		if vfs.exists(&path) {
			vfs.watch(&path, vfs.is_dir(&path))?;

			if let Some(mut path_snapshot) = new_snapshot(&path, context, vfs)? {
				path_snapshot.extend_properties(snapshot.properties);
				path_snapshot.set_name(&snapshot.name);

				if path_snapshot.class == "Folder" {
					path_snapshot.set_class(&snapshot.class);
				}

				// We want to keep the original inner source
				// but with addition of new relevant paths
				snapshot
					.meta
					.source
					.extend_relevant(path_snapshot.meta.source.relevant().to_owned());

				path_snapshot.meta.set_source(snapshot.meta.source);
				path_snapshot
					.meta
					.set_keep_unknowns(path_snapshot.meta.keep_unknowns || snapshot.meta.keep_unknowns);

				snapshot = path_snapshot;
			}
		} else if let ProjectPath::Required(_) = path_node {
			argon_warn!(
				"Path specified in the project does not exist: {}. Please create this path and restart Argon \
				to watch for file changes in this path or remove it from the project to suppress this warning",
				path.to_string().bold()
			);
		}
	}

	// `new_snapshot` above supplies metadata for the filesystem object. The
	// outer project node still owns this instance, so retain that distinction
	// after merging the two snapshots.
	snapshot.meta.set_project_owned(project_owned);

	for (node_name, node) in node.tree {
		let node_path = node_path.join(&node_name);
		let child = new_snapshot_node(&node_name, path, node, node_path, context, vfs)?;

		snapshot.add_child(child);
	}

	Ok(snapshot)
}
