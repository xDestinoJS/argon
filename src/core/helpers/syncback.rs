use colored::Colorize;
use rbx_dom_weak::{
	types::{Ref, Variant},
	ustr, HashMapExt, UstrMap,
};
use std::path::{Path, PathBuf};

use crate::{
	argon_error,
	config::Config,
	core::meta::{Meta, SyncbackFilter},
	ext::PathExt,
	resolution::UnresolvedValue,
	vfs::Vfs,
	Properties,
};

#[cfg(not(windows))]
const FORBIDDEN_CHARACTERS: [char; 1] = ['/'];

#[cfg(windows)]
const FORBIDDEN_CHARACTERS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

#[cfg(windows)]
const FORBIDDEN_FILE_NAMES: [&str; 22] = [
	"CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
	"LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn verify_name(name: &mut String, meta: &mut Meta) -> bool {
	let original_name = name.clone();
	// Recompute this marker from the current Studio name. Keeping an old value
	// after a user renames an instance to a filesystem-safe name makes the next
	// snapshot resurrect the previous name.
	meta.set_original_name(None);
	let (_, renamed) = {
		let mut messages: Vec<String> = Vec::new();
		let mut name = name.clone();

		if name.len() > 255 {
			messages.push("file name cannot be longer than 255 characters".into());
			name = name[..255].to_owned();
		}

		{
			let mut forbidden_chars = Vec::new();

			for char in name.chars() {
				if FORBIDDEN_CHARACTERS.contains(&char) && !forbidden_chars.contains(&char) {
					forbidden_chars.push(char);
				}

				#[cfg(windows)]
				if char.is_control() && !forbidden_chars.contains(&char) {
					forbidden_chars.push(char);
				}
			}

			if !forbidden_chars.is_empty() {
				for char in forbidden_chars {
					name = name.replace(char, "_");
				}
			}
		}

		#[cfg(windows)]
		if name.ends_with('.') || name.ends_with(' ') {
			while name.ends_with('.') || name.ends_with(' ') {
				name = name[..name.len() - 1].to_owned();
			}
		}

		if name.is_empty() {
			name = "EmptyName".into();
		} else {
			#[cfg(windows)]
			for file_name in FORBIDDEN_FILE_NAMES {
				if name == file_name {
					name = format!("_{}", name);
				}
			}
		}

		(messages, name)
	};

	if original_name != renamed {
		log::trace!("Instance with name: {} sanitized to: {}", original_name, renamed);
		meta.set_original_name(Some(original_name));
		*name = renamed;
	}

	true
}

/// Checks whether `originalName` could have produced the current filesystem
/// name through Argon's sanitizing or duplicate-name suffixing. Any other
/// pairing is stale metadata left behind by an earlier rename.
pub fn original_name_matches_path_name(original_name: &str, path_name: &str) -> bool {
	let mut sanitized = original_name.to_owned();
	let mut meta = Meta::new();
	verify_name(&mut sanitized, &mut meta);

	if sanitized == path_name {
		return true;
	}

	path_name.strip_prefix(&format!("{sanitized}_")).is_some_and(|suffix| {
		uuid::Uuid::parse_str(suffix).is_ok() || suffix.parse::<Ref>().is_ok_and(|referent| referent.is_some())
	})
}

pub fn verify_path(path: &mut PathBuf, name: &mut String, meta: &mut Meta, id: Ref, vfs: &Vfs) -> bool {
	if !vfs.exists(path) || meta.source.get().path().is_some_and(|p| p == path) {
		return true;
	}

	if Config::new().keep_duplicates {
		let suffix = path.get_name().strip_prefix(name.as_str()).unwrap_or_default();

		let renamed = format!("{}_{}", name, id);
		let renamed_path = path.with_file_name(format!("{renamed}{suffix}"));

		log::trace!(
			"Instance with path: {} got renamed to: {}, because it already exists!",
			path.to_string().bold(),
			renamed_path.to_string().bold()
		);

		meta.set_original_name(Some(name.to_owned()));

		*path = renamed_path;
		*name = renamed;

		true
	} else {
		argon_error!(
			"Instance with path: {} already exists! Skipping..",
			path.to_string().bold()
		);

		false
	}
}

pub fn validate_properties(properties: Properties, filter: &SyncbackFilter) -> Properties {
	// Temporary solution for empty Luau maps being serialized as arrays
	if properties.contains_key(&ustr("ArgonEmpty")) {
		UstrMap::new()
	} else {
		properties
			.into_iter()
			.filter(|(property, variant)| {
				!matches!(property.as_str(), "CanvasPosition" | "FormFactor" | "Formfactor")
					&& !matches!(variant, Variant::BinaryString(_) | Variant::SharedString(_))
					&& !filter.matches_property(property)
			})
			.collect()
	}
}

pub fn serialize_properties(class: &str, properties: Properties) -> UstrMap<UnresolvedValue> {
	properties
		.iter()
		.filter(|(property, variant)| {
			crate::util::is_persistent_property(class, property)
				&& !matches!(variant, Variant::BinaryString(_) | Variant::SharedString(_))
				&& !matches!(variant, Variant::Ref(reference) if !reference.is_some())
		})
		.map(|(property, variant)| {
			(
				*property,
				UnresolvedValue::from_variant(variant.clone(), class, property),
			)
		})
		.collect()
}

pub fn rename_path(path: &Path, from: &str, to: &str, vfs: &Vfs) -> PathBuf {
	let current_name = path.get_name();

	let clean_name = if path.is_file() {
		let file_str = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
		if let Some(idx) = file_str.find('.') {
			format!("{}{}", to, &file_str[idx..])
		} else {
			to.to_owned()
		}
	} else {
		to.to_owned()
	};

	let clean_path = path.with_file_name(&clean_name);

	if !vfs.exists(&clean_path) || clean_path == path {
		clean_path
	} else {
		path.with_file_name(format!("{}{}", to, current_name.strip_prefix(from).unwrap_or_default()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn safe_rename_clears_stale_original_name() {
		let mut name = "Right".to_owned();
		let mut meta = Meta::new().with_original_name("ImageLabel".to_owned());

		assert!(verify_name(&mut name, &mut meta));
		assert_eq!(name, "Right");
		assert_eq!(meta.original_name, None);
	}

	#[test]
	fn sanitized_name_records_the_studio_name() {
		let mut name = "Bad/Name".to_owned();
		let mut meta = Meta::new().with_original_name("OlderName".to_owned());

		assert!(verify_name(&mut name, &mut meta));
		assert_eq!(name, "Bad_Name");
		assert_eq!(meta.original_name.as_deref(), Some("Bad/Name"));
	}

	#[test]
	fn stale_original_name_does_not_match_an_unrelated_path_name() {
		assert!(!original_name_matches_path_name("ImageLabel", "Right"));
		assert!(original_name_matches_path_name("Bad/Name", "Bad_Name"));
		assert!(original_name_matches_path_name(
			"ImageLabel",
			"ImageLabel_550e8400-e29b-41d4-a716-446655440000"
		));
		assert!(original_name_matches_path_name(
			"Part",
			"Part_8afc4ed474da40c0bac118b9678e524a"
		));
	}
}
