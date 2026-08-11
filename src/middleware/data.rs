use anyhow::{Context as AnyhowContext, Result};
use log::error;
use path_clean::PathClean;
use rbx_dom_weak::{
	types::{Tags, Variant},
	ustr, HashMapExt, Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};
use std::{
	collections::{BTreeMap, HashMap, HashSet},
	fs,
	path::{Path, PathBuf},
};

use crate::{
	core::meta::Meta,
	ext::PathExt,
	middleware::helpers,
	project::{Project, ProjectNode},
	resolution::UnresolvedValue,
	util::{self, serialize_json},
	vfs::Vfs,
	Properties,
};

const SCRIPT_SUFFIXES: [&str; 6] = [
	".server.luau",
	".client.luau",
	".server.lua",
	".client.lua",
	".luau",
	".lua",
];

const LEGACY_CHILD_SCRIPT_NAMES: [&str; 6] = [
	".src.server.luau",
	".src.client.luau",
	".src.server.lua",
	".src.client.lua",
	".src.luau",
	".src.lua",
];

fn is_redundant_script_metadata(contents: &str) -> bool {
	let Ok(serde_json::Value::Object(root)) = serde_json::from_str(contents) else {
		return false;
	};

	if root.len() != 1 {
		return false;
	}

	let Some(serde_json::Value::Object(properties)) = root.get("properties") else {
		return false;
	};

	properties.len() == 1
		&& matches!(
			properties.get("Attributes"),
			Some(serde_json::Value::Object(attributes)) if attributes.is_empty()
		)
}

fn has_matching_script(meta_path: &Path) -> bool {
	let Some(file_name) = meta_path.file_name().and_then(|name| name.to_str()) else {
		return false;
	};
	let Some(script_name) = file_name.strip_suffix(".meta.json") else {
		return false;
	};
	let Some(parent) = meta_path.parent() else {
		return false;
	};

	if SCRIPT_SUFFIXES
		.iter()
		.any(|suffix| parent.join(format!("{script_name}{suffix}")).is_file())
	{
		return true;
	}

	file_name == "init.meta.json"
		&& LEGACY_CHILD_SCRIPT_NAMES
			.iter()
			.any(|script| parent.join(script).is_file())
}

fn metadata_path_for_script(script_path: &Path) -> Option<PathBuf> {
	let file_name = script_path.file_name()?.to_str()?;
	let parent = script_path.parent()?;

	if LEGACY_CHILD_SCRIPT_NAMES.contains(&file_name) {
		return Some(parent.join("init.meta.json"));
	}

	SCRIPT_SUFFIXES.iter().find_map(|suffix| {
		file_name
			.strip_suffix(suffix)
			.map(|name| parent.join(format!("{name}.meta.json")))
	})
}

fn remove_redundant_metadata(path: &Path, processed: &mut HashSet<PathBuf>, removed: &mut Vec<PathBuf>) -> Result<()> {
	if !path.is_file() || !processed.insert(path.to_owned()) || !has_matching_script(path) {
		return Ok(());
	}

	let contents =
		fs::read_to_string(path).with_context(|| format!("Failed to inspect script metadata at {}", path.display()))?;

	if !is_redundant_script_metadata(&contents) {
		return Ok(());
	}

	fs::remove_file(path)
		.with_context(|| format!("Failed to remove redundant script metadata at {}", path.display()))?;
	removed.push(path.to_owned());

	Ok(())
}

fn scan_for_redundant_script_metadata(
	path: &Path,
	processed: &mut HashSet<PathBuf>,
	removed: &mut Vec<PathBuf>,
) -> Result<()> {
	if path.is_file() {
		if let Some(meta_path) = metadata_path_for_script(path) {
			remove_redundant_metadata(&meta_path, processed, removed)?;
		}
		return Ok(());
	}

	if !path.is_dir() {
		return Ok(());
	}

	for entry in fs::read_dir(path).with_context(|| format!("Failed to scan project path {}", path.display()))? {
		let entry = entry?;
		let file_type = entry.file_type()?;
		let entry_path = entry.path();

		if file_type.is_dir() {
			let name = entry.file_name();
			let name = name.to_string_lossy();
			if matches!(name.as_ref(), ".git" | ".hg" | ".svn" | "node_modules" | "target") {
				continue;
			}
			scan_for_redundant_script_metadata(&entry_path, processed, removed)?;
		} else if file_type.is_file()
			&& entry_path
				.file_name()
				.and_then(|name| name.to_str())
				.is_some_and(|name| name.ends_with(".meta.json"))
		{
			remove_redundant_metadata(&entry_path, processed, removed)?;
		}
	}

	Ok(())
}

/// Remove script-adjacent metadata files that contain no information besides
/// an empty Attributes map. Other metadata files are always preserved.
pub fn cleanup_redundant_script_metadata(project: &Project) -> Result<Vec<PathBuf>> {
	fn collect_paths(node: &ProjectNode, workspace: &Path, paths: &mut HashSet<PathBuf>) {
		if let Some(source) = &node.path {
			let source = if source.path().is_absolute() {
				source.path().to_owned()
			} else {
				workspace.join(source.path())
			}
			.clean();

			// Startup cleanup is deliberately limited to the current workspace.
			if source.starts_with(workspace) {
				paths.insert(source);
			}
		}

		for child in node.tree.values() {
			collect_paths(child, workspace, paths);
		}
	}

	let workspace = project.workspace_dir.clean();
	let mut source_paths = HashSet::new();
	collect_paths(&project.node, &workspace, &mut source_paths);

	let mut processed = HashSet::new();
	let mut removed = Vec::new();
	for path in source_paths {
		scan_for_redundant_script_metadata(&path, &mut processed, &mut removed)?;
	}

	removed.sort();
	Ok(removed)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
	class_name: Option<Ustr>,

	#[serde(default)]
	properties: HashMap<Ustr, UnresolvedValue>,
	attributes: Option<UnresolvedValue>,
	#[serde(default)]
	tags: Vec<String>,

	#[serde(alias = "ignoreUnknownInstances", default)]
	keep_unknowns: Option<bool>,
	#[serde(default)]
	original_name: Option<String>,
}

#[derive(Debug, Default)]
pub struct DataSnapshot {
	pub path: PathBuf,
	pub class: Option<Ustr>,
	pub properties: Properties,
	pub keep_unknowns: Option<bool>,
	pub original_name: Option<String>,
	pub mesh_source: Option<String>,
}

#[profiling::function]
pub fn read_data(path: &Path, class: Option<&str>, vfs: &Vfs) -> Result<DataSnapshot> {
	let data = vfs.read_to_string(path)?;

	if data.is_empty() {
		return Ok(DataSnapshot::default());
	}

	let data: Data = serde_json::from_str(&data)?;

	let mut properties = UstrMap::new();

	let class = if let Some(class) = class.or(data.class_name.as_deref()) {
		class.to_owned()
	} else {
		let name = path.get_name();

		if util::is_service(name) {
			name.to_owned()
		} else {
			let parent_name = path.get_parent().get_name();

			if util::is_service(parent_name) {
				parent_name.to_owned()
			} else {
				String::from("Folder")
			}
		}
	};

	// Resolve properties
	for (property, value) in data.properties {
		match value.resolve(&class, &property) {
			Ok(value) => {
				properties.insert(property, value);
			}
			Err(err) => {
				error!("Failed to parse property: {} at {}", err, path.display());
			}
		}
	}

	// Resolve attributes
	if let Some(attributes) = data.attributes {
		match attributes.resolve(&class, "Attributes") {
			Ok(value) => {
				properties.insert(ustr("Attributes"), value);
			}
			Err(err) => {
				error!("Failed to parse attributes: {} at {}", err, path.display());
			}
		}
	}

	// Resolve tags
	if !data.tags.is_empty() {
		properties.insert(ustr("Tags"), Tags::from(data.tags).into());
	}

	let mesh_source = if class == "MeshPart" {
		helpers::save_mesh(&properties)
	} else {
		None
	};

	Ok(DataSnapshot {
		path: path.to_owned(),
		class: data.class_name,
		properties,
		keep_unknowns: data.keep_unknowns,
		original_name: data.original_name,
		mesh_source,
	})
}

#[derive(Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct WritableData {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub class_name: Option<Ustr>,
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub properties: BTreeMap<Ustr, UnresolvedValue>,

	#[serde(skip_serializing_if = "Option::is_none")]
	pub keep_unknowns: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub original_name: Option<String>,
}

#[profiling::function]
pub fn write_data<'a>(
	has_file: bool,
	class: &str,
	properties: Properties,
	path: &'a Path,
	meta: &Meta,
	vfs: &Vfs,
) -> Result<Option<&'a Path>> {
	let class_name = if !has_file && class != "Folder" {
		Some(Ustr::from(class))
	} else {
		None
	};

	let properties: BTreeMap<Ustr, UnresolvedValue> = properties
		.iter()
		.filter(|(property, variant)| {
			!matches!(variant, Variant::Ref(reference) if !reference.is_some())
				&& !(has_file
					&& util::is_script(class)
					&& **property == ustr("Attributes")
					&& matches!(variant, Variant::Attributes(attributes) if attributes.is_empty()))
		})
		.map(|(property, variant)| {
			(
				*property,
				UnresolvedValue::from_variant(variant.clone(), class, property),
			)
		})
		.collect();
	let mut data = WritableData {
		class_name,
		properties,
		original_name: meta.original_name.clone(),
		..WritableData::default()
	};

	if meta.keep_unknowns {
		data.keep_unknowns = Some(true);
	}

	if let Some(original_name) = meta.original_name.as_ref() {
		data.original_name = Some(original_name.to_owned());
	}

	if data == WritableData::default() {
		if vfs.exists(path) {
			vfs.remove(path)?;
		}

		return Ok(None);
	}

	vfs.write(path, &serialize_json(&data)?)?;

	Ok(Some(path))
}

#[profiling::function]
pub fn write_original_name(path: &Path, meta: &Meta, vfs: &Vfs) -> Result<()> {
	let data = if vfs.exists(path) {
		let data = vfs.read_to_string(path)?;

		if data.is_empty() {
			return Ok(());
		}

		let data: Data = serde_json::from_str(&data)?;

		if data.original_name == meta.original_name {
			return Ok(());
		}

		let data = WritableData {
			class_name: data.class_name,
			properties: data.properties.into_iter().collect(),
			keep_unknowns: data.keep_unknowns,
			original_name: meta.original_name.clone(),
		};

		if data == WritableData::default() {
			vfs.remove(path)?;
			return Ok(());
		}

		data
	} else {
		let data = WritableData {
			original_name: meta.original_name.clone(),
			..WritableData::default()
		};

		if data == WritableData::default() {
			return Ok(());
		}

		data
	};

	vfs.write(path, &serialize_json(&data)?)?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::types::Attributes;
	use uuid::Uuid;

	#[test]
	fn script_data_omits_empty_attributes() {
		let vfs = Vfs::new_virtual();
		let path = Path::new("src/Test.meta.json");
		vfs.create_dir(Path::new("src")).unwrap();
		vfs.write(path, br#"{"properties":{"Attributes":{}}}"#).unwrap();

		let mut properties = Properties::default();
		properties.insert(ustr("Attributes"), Variant::Attributes(Attributes::new()));

		let result = write_data(true, "ModuleScript", properties, path, &Meta::new(), &vfs).unwrap();
		assert!(result.is_none());
		assert!(!vfs.exists(path));
	}

	#[test]
	fn script_data_keeps_other_properties_while_omitting_empty_attributes() {
		let vfs = Vfs::new_virtual();
		let path = Path::new("src/Test.meta.json");
		vfs.create_dir(Path::new("src")).unwrap();

		let mut properties = Properties::default();
		properties.insert(ustr("Attributes"), Variant::Attributes(Attributes::new()));
		properties.insert(ustr("Disabled"), Variant::Bool(true));

		assert!(write_data(true, "ModuleScript", properties, path, &Meta::new(), &vfs)
			.unwrap()
			.is_some());

		let data: serde_json::Value = serde_json::from_str(&vfs.read_to_string(path).unwrap()).unwrap();
		let properties = data.get("properties").unwrap().as_object().unwrap();
		assert!(!properties.contains_key("Attributes"));
		assert_eq!(properties.get("Disabled"), Some(&serde_json::Value::Bool(true)));
	}

	#[test]
	fn startup_cleanup_only_removes_exact_empty_script_attributes_metadata() {
		let root = std::env::temp_dir().join(format!("argon-script-meta-cleanup-{}", Uuid::new_v4()));
		let src = root.join("src");
		let legacy = src.join("Legacy");
		fs::create_dir_all(&legacy).unwrap();

		let project_path = root.join("default.project.json");
		fs::write(
			&project_path,
			r#"{"name":"test","tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}}}"#,
		)
		.unwrap();

		fs::write(src.join("Module.luau"), "return {}").unwrap();
		fs::write(src.join("Module.meta.json"), r#"{"properties":{"Attributes":{}}}"#).unwrap();
		fs::write(src.join("Client.client.luau"), "print('client')").unwrap();
		fs::write(
			src.join("Client.meta.json"),
			"{\n  \"properties\": {\n    \"Attributes\": {}\n  }\n}",
		)
		.unwrap();
		fs::write(legacy.join(".src.server.lua"), "print('legacy')").unwrap();
		fs::write(legacy.join("init.meta.json"), r#"{"properties":{"Attributes":{}}}"#).unwrap();

		fs::write(src.join("Meaningful.server.luau"), "print('server')").unwrap();
		fs::write(
			src.join("Meaningful.meta.json"),
			r#"{"properties":{"Attributes":{},"Disabled":true}}"#,
		)
		.unwrap();
		fs::write(src.join("NonEmpty.luau"), "return {}").unwrap();
		fs::write(
			src.join("NonEmpty.meta.json"),
			r#"{"properties":{"Attributes":{"Mode":"Test"}}}"#,
		)
		.unwrap();
		fs::write(src.join("Orphan.meta.json"), r#"{"properties":{"Attributes":{}}}"#).unwrap();

		let project = Project::load(&project_path).unwrap();
		let removed = cleanup_redundant_script_metadata(&project).unwrap();

		assert_eq!(removed.len(), 3);
		assert!(!src.join("Module.meta.json").exists());
		assert!(!src.join("Client.meta.json").exists());
		assert!(!legacy.join("init.meta.json").exists());
		assert!(src.join("Meaningful.meta.json").exists());
		assert!(src.join("NonEmpty.meta.json").exists());
		assert!(src.join("Orphan.meta.json").exists());

		fs::remove_dir_all(root).unwrap();
	}
}
