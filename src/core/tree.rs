use log::{error, trace};
use multimap::MultiMap;
use rbx_dom_weak::{
	types::{Ref, Variant},
	ustr, Instance, InstanceBuilder, WeakDom,
};
use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use super::{meta::Meta, snapshot::Snapshot};

#[derive(Debug)]
pub struct Tree {
	dom: WeakDom,
	path_to_ids: MultiMap<PathBuf, Ref>,
	id_to_meta: HashMap<Ref, Meta>,
}

impl Tree {
	pub fn new(mut snapshot: Snapshot) -> Self {
		let persisted_id = persisted_argon_id(&snapshot);
		let mut builder = InstanceBuilder::new(snapshot.class)
			.with_name(snapshot.name)
			.with_properties(snapshot.properties);

		if let Some(id) = persisted_id {
			builder = builder.with_referent(id);
		}
		snapshot.id = builder.referent();

		let mut tree = Self {
			dom: WeakDom::new(builder),
			id_to_meta: HashMap::new(),
			path_to_ids: MultiMap::new(),
		};

		let root_ref = tree.dom.root_ref();

		tree.insert_meta(root_ref, snapshot.meta);

		for child in snapshot.children {
			if !crate::constants::is_ignored_class(&child.class) {
				tree.insert_instance_recursive(child, root_ref);
			}
		}

		tree
	}

	pub fn insert_instance(&mut self, mut snapshot: Snapshot, parent: Ref) -> Ref {
		let id = self.disk_referent(&snapshot);
		snapshot.id = id;
		let builder = InstanceBuilder::new(snapshot.class)
			.with_name(snapshot.meta.original_name.as_ref().unwrap_or(&snapshot.name))
			.with_referent(id)
			.with_properties(snapshot.properties);

		let inserted_id = self.dom.insert(parent, builder);

		self.insert_meta(inserted_id, snapshot.meta);

		inserted_id
	}

	pub fn insert_instance_recursive(&mut self, mut snapshot: Snapshot, parent: Ref) -> Ref {
		let id = self.disk_referent(&snapshot);
		snapshot.id = id;
		let builder = InstanceBuilder::new(snapshot.class)
			.with_name(snapshot.meta.original_name.as_ref().unwrap_or(&snapshot.name))
			.with_referent(id)
			.with_properties(snapshot.properties);

		let inserted_id = self.dom.insert(parent, builder);

		self.insert_meta(inserted_id, snapshot.meta);

		for child in snapshot.children {
			if !crate::constants::is_ignored_class(&child.class) {
				self.insert_instance_recursive(child, inserted_id);
			}
		}

		inserted_id
	}

	pub fn insert_instance_with_ref(&mut self, snapshot: Snapshot, parent: Ref) {
		if self.exists(snapshot.id) {
			trace!(
				"Studio instance '{}' with duplicate ArgonId {}; keeping existing DOM entry",
				snapshot.name,
				snapshot.id
			);
			return;
		}

		let builder = InstanceBuilder::new(snapshot.class)
			.with_name(snapshot.meta.original_name.as_ref().unwrap_or(&snapshot.name))
			.with_referent(snapshot.id)
			.with_properties(snapshot.properties);

		let id = self.dom.insert(parent, builder);

		self.insert_meta(id, snapshot.meta);
	}

	fn disk_referent(&self, snapshot: &Snapshot) -> Ref {
		let Some(id) = persisted_argon_id(snapshot) else {
			return self.fresh_referent();
		};

		if !self.exists(id) {
			return id;
		}

		let existing = self.get_instance(id);
		let existing_meta = self.get_meta(id);
		trace!(
			"Duplicate persisted ArgonId {id} for disk instance '{}' from {:?}; it is already owned by '{}' from {:?}. Keeping first disk instance and assigning duplicate fresh in-memory referent",
			snapshot.name,
			snapshot.meta.source,
			existing.map(|instance| instance.name.as_str()).unwrap_or("<unknown>"),
			existing_meta.map(|meta| &meta.source),
		);

		self.fresh_referent()
	}

	fn fresh_referent(&self) -> Ref {
		loop {
			let id = Ref::new();
			if !self.exists(id) {
				return id;
			}
		}
	}

	pub fn remove_instance(&mut self, id: Ref) {
		if id == self.root_ref() {
			error!("Attempted to remove root instance: {id:?}");
			return;
		}

		let mut to_remove = vec![id];

		fn walk(id: Ref, dom: &WeakDom, to_remove: &mut Vec<Ref>) {
			let Some(instance) = dom.get_by_ref(id) else {
				return;
			};

			for child in instance.children() {
				to_remove.push(*child);
				walk(*child, dom, to_remove);
			}
		}

		walk(id, &self.dom, &mut to_remove);

		for id in to_remove {
			self.remove_meta(id);
		}

		self.dom.destroy(id);
	}

	pub fn get_instance(&self, id: Ref) -> Option<&Instance> {
		self.dom.get_by_ref(id)
	}

	pub fn get_instance_mut(&mut self, id: Ref) -> Option<&mut Instance> {
		self.dom.get_by_ref_mut(id)
	}

	pub fn insert_meta(&mut self, id: Ref, meta: Meta) -> Option<Meta> {
		for path in meta.source.paths() {
			self.path_to_ids.insert(path.to_owned(), id);
		}

		self.id_to_meta.insert(id, meta)
	}

	pub fn update_meta(&mut self, id: Ref, meta: Meta) -> Option<Meta> {
		let old_meta = self.id_to_meta.remove(&id);

		if let Some(old_meta) = &old_meta {
			let removed: Vec<&Path> = old_meta
				.source
				.paths()
				.into_iter()
				.filter(|&path| !meta.source.paths().contains(&path))
				.collect();

			let added: Vec<&Path> = meta
				.source
				.paths()
				.into_iter()
				.filter(|&path| !old_meta.source.paths().contains(&path))
				.collect();

			for path in removed {
				self.path_to_ids.remove(path);
			}

			for path in added {
				self.path_to_ids.insert(path.to_owned(), id);
			}
		}

		self.id_to_meta.insert(id, meta);

		old_meta
	}

	pub fn remove_meta(&mut self, id: Ref) -> Option<Meta> {
		let meta = self.id_to_meta.remove(&id);

		if let Some(meta) = &meta {
			for path in meta.source.paths() {
				self.path_to_ids.remove(path);
			}
		}

		meta
	}

	pub fn get_meta(&self, id: Ref) -> Option<&Meta> {
		self.id_to_meta.get(&id)
	}

	pub fn get_meta_mut(&mut self, id: Ref) -> Option<&mut Meta> {
		self.id_to_meta.get_mut(&id)
	}

	pub fn get_ids(&self, path: &Path) -> Option<&Vec<Ref>> {
		self.path_to_ids.get_vec(path)
	}

	pub fn exists(&self, id: Ref) -> bool {
		self.dom.get_by_ref(id).is_some()
	}

	pub fn inner(&self) -> &WeakDom {
		&self.dom
	}

	pub fn meta_map(&self) -> &HashMap<Ref, Meta> {
		&self.id_to_meta
	}

	pub fn id_map(&self) -> &MultiMap<PathBuf, Ref> {
		&self.path_to_ids
	}

	pub fn root(&self) -> &Instance {
		self.dom.root()
	}

	pub fn root_ref(&self) -> Ref {
		self.dom.root_ref()
	}

	pub fn place_root_refs(&self) -> &[Ref] {
		self.dom.root().children()
	}
}

pub(crate) fn persisted_argon_id(snapshot: &Snapshot) -> Option<Ref> {
	let Variant::Attributes(attributes) = snapshot.properties.get(&ustr("Attributes"))? else {
		return None;
	};
	let Variant::String(raw_id) = attributes.get("ArgonId")? else {
		return None;
	};

	let normalized = raw_id
		.chars()
		.filter(|character| !matches!(character, '{' | '}' | '-'))
		.collect::<String>()
		.to_ascii_lowercase();

	if normalized.len() != 32 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
		return None;
	}

	normalized.parse::<Ref>().ok().filter(Ref::is_some)
}

#[cfg(test)]
mod tests {
	use super::Tree;
	use crate::core::snapshot::Snapshot;

	#[test]
	fn ignored_classes_are_pruned_from_nested_snapshots() {
		let snapshot = Snapshot::new().with_class("DataModel").with_children(vec![
			Snapshot::new().with_name("Legacy joint").with_class("Snap"),
			Snapshot::new().with_name("Kept part").with_class("Part"),
		]);

		let tree = Tree::new(snapshot);
		let children = tree.root().children();
		assert_eq!(children.len(), 1);
		assert_eq!(tree.get_instance(children[0]).unwrap().class, "Part");
	}
}
