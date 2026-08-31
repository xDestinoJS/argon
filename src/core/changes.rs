use rbx_dom_weak::types::Ref;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::snapshot::{AddedSnapshot, Snapshot, UpdatedSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changes {
	pub additions: Vec<AddedSnapshot>,
	pub updates: Vec<UpdatedSnapshot>,
	pub removals: Vec<Ref>,
}

impl Changes {
	pub fn new() -> Self {
		Self {
			additions: Vec::new(),
			updates: Vec::new(),
			removals: Vec::new(),
		}
	}

	pub fn add(&mut self, snapshot: Snapshot, parent: Ref) {
		self.additions.push(AddedSnapshot {
			id: snapshot.id,
			parent,
			name: snapshot.name,
			class: snapshot.class,
			properties: snapshot.properties,
			children: snapshot.children,
			meta: snapshot.meta,
		});
	}

	pub fn update(&mut self, modified_snapshot: UpdatedSnapshot) {
		self.updates.push(modified_snapshot);
	}

	pub fn remove(&mut self, id: Ref) {
		self.removals.push(id);
	}

	pub fn extend(&mut self, changes: Self) {
		self.additions.extend(changes.additions);
		self.updates.extend(changes.updates);
		self.removals.extend(changes.removals);
	}

	pub fn is_empty(&self) -> bool {
		self.additions.is_empty() && self.updates.is_empty() && self.removals.is_empty()
	}

	pub fn total(&self) -> usize {
		self.additions.len() + self.updates.len() + self.removals.len()
	}

	pub fn is_update_only(&self) -> bool {
		self.additions.is_empty() && self.removals.is_empty()
	}

	/// Collapse newer property checkpoints into an update-only batch. This is
	/// used after a very large transform has kept the disk writer busy: every
	/// journal entry remains durable until the combined final state is written,
	/// but the same 6,000 files do not need to be rewritten for every checkpoint.
	pub fn coalesce_updates(&mut self, other: Self) {
		debug_assert!(self.is_update_only() && other.is_update_only());
		let mut indexes = self
			.updates
			.iter()
			.enumerate()
			.map(|(index, snapshot)| (snapshot.id, index))
			.collect::<HashMap<_, _>>();

		for mut incoming in other.updates {
			if let Some(index) = indexes.get(&incoming.id).copied() {
				let existing = &mut self.updates[index];

				if let Some(properties) = incoming.properties.take() {
					if incoming.partial_properties {
						if let Some(existing_properties) = existing.properties.as_mut() {
							existing_properties.extend(properties);
						} else {
							existing.properties = Some(properties);
							existing.partial_properties = true;
						}
					} else {
						existing.properties = Some(properties);
						existing.partial_properties = false;
					}
				}

				if incoming.name.is_some() {
					existing.name = incoming.name;
				}
				if incoming.class.is_some() {
					existing.class = incoming.class;
				}
				if incoming.meta.is_some() {
					existing.meta = incoming.meta;
				}
			} else {
				indexes.insert(incoming.id, self.updates.len());
				self.updates.push(incoming);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::{types::Variant, ustr};

	#[test]
	fn update_checkpoints_keep_only_the_latest_property_value() {
		let id = Ref::new();
		let mut first_snapshot = UpdatedSnapshot::new(id);
		first_snapshot.partial_properties = true;
		let mut first_properties = crate::Properties::default();
		first_properties.insert(ustr("CFrame"), Variant::String("first".into()));
		first_snapshot.properties = Some(first_properties);
		let mut first = Changes::new();
		first.update(first_snapshot);

		let mut second_snapshot = UpdatedSnapshot::new(id);
		second_snapshot.partial_properties = true;
		let mut second_properties = crate::Properties::default();
		second_properties.insert(ustr("CFrame"), Variant::String("final".into()));
		second_snapshot.properties = Some(second_properties);
		let mut second = Changes::new();
		second.update(second_snapshot);

		first.coalesce_updates(second);

		assert_eq!(first.updates.len(), 1);
		assert_eq!(
			first.updates[0].properties.as_ref().unwrap().get(&ustr("CFrame")),
			Some(&Variant::String("final".into()))
		);
		assert!(first.updates[0].partial_properties);
	}
}
