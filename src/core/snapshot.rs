use rbx_dom_weak::{
	types::{Ref, Variant},
	ustr, HashMapExt, Ustr, UstrMap,
};
use serde::{
	de::{Error as DeError, IgnoredAny, MapAccess, SeqAccess, Visitor},
	Deserialize, Deserializer, Serialize,
};
use std::fmt::{self, Debug, Formatter};

use super::{
	helpers::{apply_migrations, syncback::original_name_matches_path_name},
	meta::Meta,
};
use crate::{middleware::data::DataSnapshot, Properties};

fn normalize_empty_variant_maps(value: &mut rmpv::Value) {
	let rmpv::Value::Map(variant) = value else {
		return;
	};

	for (kind, payload) in variant {
		let Some(kind) = kind.as_str() else {
			continue;
		};

		if matches!(kind, "Attributes" | "MaterialColors")
			&& matches!(payload, rmpv::Value::Array(items) if items.is_empty())
		{
			*payload = rmpv::Value::Map(Vec::new());
		}

		if kind == "Attributes" {
			if let rmpv::Value::Map(attributes) = payload {
				for (_, attribute) in attributes {
					normalize_empty_variant_maps(attribute);
				}
			}
		}
	}
}

fn deserialize_properties<'de, D>(deserializer: D) -> Result<Properties, D::Error>
where
	D: Deserializer<'de>,
{
	struct PropertiesVisitor;

	impl<'de> Visitor<'de> for PropertiesVisitor {
		type Value = Properties;

		fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
			formatter.write_str("a property map or an empty sequence")
		}

		fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
		where
			A: MapAccess<'de>,
		{
			let mut properties = Properties::default();
			while let Some((name, mut value)) = map.next_entry::<Ustr, rmpv::Value>()? {
				normalize_empty_variant_maps(&mut value);
				let mut encoded = Vec::new();
				rmpv::encode::write_value(&mut encoded, &value).map_err(A::Error::custom)?;
				let value = match rmp_serde::from_slice(&encoded) {
					Ok(value) => value,
					Err(rmp_serde::decode::Error::Syntax(message))
						if message.contains("invalid value: byte array, expected a string") =>
					{
						log::debug!("Skipping property '{name}' because Studio sent binary data for a string value");
						continue;
					}
					Err(error) => {
						return Err(A::Error::custom(format!("property '{name}': {error}")));
					}
				};
				properties.insert(name, value);
			}
			Ok(properties)
		}

		fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
		where
			A: SeqAccess<'de>,
		{
			if sequence.next_element::<IgnoredAny>()?.is_some() {
				return Err(A::Error::custom("only an empty sequence can represent a property map"));
			}

			Ok(Properties::default())
		}
	}

	deserializer.deserialize_any(PropertiesVisitor)
}

fn deserialize_optional_properties<'de, D>(deserializer: D) -> Result<Option<Properties>, D::Error>
where
	D: Deserializer<'de>,
{
	struct OptionalPropertiesVisitor;

	impl<'de> Visitor<'de> for OptionalPropertiesVisitor {
		type Value = Option<Properties>;

		fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
			formatter.write_str("null, a property map, or an empty sequence")
		}

		fn visit_none<E>(self) -> Result<Self::Value, E>
		where
			E: DeError,
		{
			Ok(None)
		}

		fn visit_unit<E>(self) -> Result<Self::Value, E>
		where
			E: DeError,
		{
			Ok(None)
		}

		fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
		where
			D: Deserializer<'de>,
		{
			deserialize_properties(deserializer).map(Some)
		}
	}

	deserializer.deserialize_option(OptionalPropertiesVisitor)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
	pub id: Ref,
	pub meta: Meta,

	// Roblox related
	pub name: String,
	pub class: Ustr,
	#[serde(deserialize_with = "deserialize_properties")]
	pub properties: Properties,
	pub children: Vec<Snapshot>,
}

impl Snapshot {
	// Creating new snapshot

	pub fn new() -> Self {
		Self {
			id: Ref::none(),
			meta: Meta::new(),
			name: String::new(),
			class: Ustr::from("Folder"),
			properties: UstrMap::new(),
			children: Vec::new(),
		}
	}

	pub fn with_id(mut self, id: Ref) -> Self {
		self.set_id(id);
		self
	}

	pub fn with_meta(mut self, meta: Meta) -> Self {
		self.set_meta(meta);
		self
	}

	pub fn with_name(mut self, name: &str) -> Self {
		self.set_name(name);
		self
	}

	pub fn with_class(mut self, class: &str) -> Self {
		self.set_class(class);
		self
	}

	pub fn with_properties(mut self, properties: Properties) -> Self {
		self.set_properties(properties);
		self
	}

	pub fn with_children(mut self, children: Vec<Snapshot>) -> Self {
		self.set_children(children);
		self
	}

	pub fn with_data(mut self, data: DataSnapshot) -> Self {
		self.apply_data(data);
		self
	}

	// Overwriting snapshot fields

	pub fn set_id(&mut self, id: Ref) {
		self.id = id;
	}

	pub fn set_meta(&mut self, meta: Meta) {
		self.meta = meta;
	}

	pub fn set_name(&mut self, name: &str) {
		name.clone_into(&mut self.name);
	}

	pub fn set_class(&mut self, class: &str) {
		self.class = class.into();
	}

	pub fn set_properties(&mut self, properties: Properties) {
		self.properties = properties;
		apply_migrations(&self.class, &mut self.properties);
	}

	pub fn set_children(&mut self, children: Vec<Snapshot>) {
		self.children = children;
	}

	pub fn apply_data(&mut self, data: DataSnapshot) {
		if let Some(class) = data.class {
			self.class = class;
		}

		if let Some(keep_unknowns) = data.keep_unknowns {
			self.meta.keep_unknowns = keep_unknowns;
		}

		if let Some(original_name) = data.original_name {
			if original_name_matches_path_name(&original_name, &self.name) {
				self.name = original_name.clone();
				self.meta.set_original_name(Some(original_name));
			} else {
				log::warn!(
					"Ignoring stale originalName {:?} for filesystem instance {:?}",
					original_name,
					self.name
				);
			}
		}

		if let Some(mesh_source) = data.mesh_source {
			self.meta.set_mesh_source(Some(mesh_source));
		}

		self.extend_properties(data.properties);
		self.meta.source.add_data(&data.path);
	}

	// Adding to snapshot fields

	pub fn add_property(&mut self, name: &str, value: Variant) {
		self.properties.insert(name.into(), value);
		apply_migrations(&self.class, &mut self.properties);
	}

	pub fn add_child(&mut self, child: Snapshot) {
		self.children.push(child);
	}

	// Joining snapshot fields

	pub fn extend_properties(&mut self, properties: Properties) {
		self.properties.extend(properties);
		apply_migrations(&self.class, &mut self.properties);
	}

	pub fn extend_children(&mut self, children: Vec<Snapshot>) {
		self.children.extend(children);
	}

	// Miscellaneous

	pub fn as_new(&self, parent: Ref) -> AddedSnapshot {
		AddedSnapshot {
			id: self.id,
			meta: self.meta.clone(),
			parent,
			name: self.name.clone(),
			class: self.class,
			properties: self.properties.clone(),
			children: self.children.clone(),
		}
	}
}

impl Debug for Snapshot {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		let mut debug = f.debug_struct("Snapshot");

		debug.field("name", &self.name);
		debug.field("class", &self.class);
		debug.field("id", &self.id);
		debug.field("meta", &self.meta);

		if !self.properties.is_empty() {
			let mut properties = self.properties.clone();

			if let Some(property) = properties.get_mut(&ustr("Source")) {
				if let Variant::String(source) = property {
					let lines = source.lines().count();

					if lines > 1 {
						*property = Variant::String(format!("Truncated... ({lines} lines)"));
					}
				}
			}

			debug.field("properties", &properties);
		}

		if !self.children.is_empty() {
			debug.field("children", &self.children);
		}

		debug.finish()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddedSnapshot {
	pub id: Ref,
	pub meta: Meta,
	pub parent: Ref,
	pub name: String,
	pub class: Ustr,
	#[serde(deserialize_with = "deserialize_properties")]
	pub properties: Properties,
	pub children: Vec<Snapshot>,
}

impl From<AddedSnapshot> for Snapshot {
	fn from(snapshot: AddedSnapshot) -> Self {
		Self {
			id: snapshot.id,
			meta: snapshot.meta,
			name: snapshot.name,
			class: snapshot.class,
			properties: snapshot.properties,
			children: snapshot.children,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatedSnapshot {
	pub id: Ref,
	pub meta: Option<Meta>,
	pub name: Option<String>,
	pub class: Option<Ustr>,
	#[serde(default, deserialize_with = "deserialize_optional_properties")]
	pub properties: Option<Properties>,
	#[serde(rename = "partialProperties", default)]
	pub partial_properties: bool,
}

impl UpdatedSnapshot {
	pub fn new(id: Ref) -> Self {
		Self {
			id,
			name: None,
			class: None,
			properties: None,
			meta: None,
			partial_properties: false,
		}
	}

	pub fn is_empty(&self) -> bool {
		self.meta.is_none() && self.name.is_none() && self.class.is_none() && self.properties.is_none()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Serialize)]
	struct LuaUpdatedSnapshot {
		id: Ref,
		properties: rmpv::Value,
	}

	#[test]
	fn empty_lua_sequence_deserializes_as_empty_properties_map() {
		let encoded = rmp_serde::to_vec_named(&LuaUpdatedSnapshot {
			id: Ref::new(),
			properties: rmpv::Value::Array(Vec::new()),
		})
		.unwrap();

		let snapshot: UpdatedSnapshot = rmp_serde::from_slice(&encoded).unwrap();
		assert!(snapshot.properties.unwrap().is_empty());
	}

	#[test]
	fn non_empty_sequence_is_rejected_as_properties_map() {
		let encoded = rmp_serde::to_vec_named(&LuaUpdatedSnapshot {
			id: Ref::new(),
			properties: rmpv::Value::Array(vec![rmpv::Value::Nil]),
		})
		.unwrap();

		assert!(rmp_serde::from_slice::<UpdatedSnapshot>(&encoded).is_err());
	}

	#[test]
	fn regular_properties_still_deserialize_through_compatibility_layer() {
		let encoded = rmp_serde::to_vec_named(&LuaUpdatedSnapshot {
			id: Ref::new(),
			properties: rmpv::Value::Map(vec![(
				rmpv::Value::from("Rotation"),
				rmpv::Value::Map(vec![(rmpv::Value::from("Float64"), rmpv::Value::from(45.0))]),
			)]),
		})
		.unwrap();

		let snapshot: UpdatedSnapshot = rmp_serde::from_slice(&encoded).unwrap();
		let rotation = snapshot.properties.unwrap().remove(&ustr("Rotation")).unwrap();
		assert_eq!(rotation, Variant::Float64(45.0));
	}

	#[test]
	fn partial_properties_flag_deserializes_from_lua_camel_case() {
		#[derive(Serialize)]
		#[serde(rename_all = "camelCase")]
		struct LuaPartialUpdatedSnapshot {
			id: Ref,
			properties: rmpv::Value,
			partial_properties: bool,
		}

		let encoded = rmp_serde::to_vec_named(&LuaPartialUpdatedSnapshot {
			id: Ref::new(),
			properties: rmpv::Value::Map(Vec::new()),
			partial_properties: true,
		})
		.unwrap();

		let snapshot: UpdatedSnapshot = rmp_serde::from_slice(&encoded).unwrap();
		assert!(snapshot.partial_properties);
	}

	#[test]
	fn binary_backed_string_property_does_not_reject_the_snapshot() {
		let encoded = rmp_serde::to_vec_named(&LuaUpdatedSnapshot {
			id: Ref::new(),
			properties: rmpv::Value::Map(vec![
				(
					rmpv::Value::from("Malformed"),
					rmpv::Value::Map(vec![(rmpv::Value::from("String"), rmpv::Value::Binary(vec![0xff]))]),
				),
				(
					rmpv::Value::from("Rotation"),
					rmpv::Value::Map(vec![(rmpv::Value::from("Float64"), rmpv::Value::from(45.0))]),
				),
			]),
		})
		.unwrap();

		let snapshot: UpdatedSnapshot = rmp_serde::from_slice(&encoded).unwrap();
		let properties = snapshot.properties.unwrap();
		assert!(!properties.contains_key(&ustr("Malformed")));
		assert_eq!(properties.get(&ustr("Rotation")), Some(&Variant::Float64(45.0)));
	}

	#[test]
	fn empty_lua_attributes_sequence_deserializes_as_empty_attributes_map() {
		let encoded = rmp_serde::to_vec_named(&LuaUpdatedSnapshot {
			id: Ref::new(),
			properties: rmpv::Value::Map(vec![(
				rmpv::Value::from("Attributes"),
				rmpv::Value::Map(vec![(rmpv::Value::from("Attributes"), rmpv::Value::Array(Vec::new()))]),
			)]),
		})
		.unwrap();

		let snapshot: UpdatedSnapshot = rmp_serde::from_slice(&encoded).unwrap();
		let attributes = snapshot.properties.unwrap().remove(&ustr("Attributes")).unwrap();
		assert!(matches!(attributes, Variant::Attributes(values) if values.is_empty()));
	}
}
