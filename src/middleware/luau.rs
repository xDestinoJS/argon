use anyhow::Result;
use rbx_dom_weak::{
	types::{Enum, Variant},
	ustr, HashMapExt, UstrMap,
};
use std::path::Path;

use super::Middleware;
use crate::{
	core::{meta::Context, snapshot::Snapshot},
	vfs::Vfs,
	Properties,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ScriptType {
	Server,
	Client,
	Module,
}

impl From<Middleware> for ScriptType {
	fn from(middleware: Middleware) -> Self {
		match middleware {
			Middleware::ServerScript => ScriptType::Server,
			Middleware::ClientScript => ScriptType::Client,
			Middleware::ModuleScript => ScriptType::Module,
			_ => panic!("Cannot convert {middleware:?} to ScriptType"),
		}
	}
}

#[profiling::function]
pub fn read_luau(path: &Path, context: &Context, vfs: &Vfs, script_type: ScriptType) -> Result<Snapshot> {
	let (class_name, run_context) = match (context.use_legacy_scripts(), &script_type) {
		(false, ScriptType::Server) => ("Script", Some(Variant::Enum(Enum::from_u32(1)))),
		(false, ScriptType::Client) => ("Script", Some(Variant::Enum(Enum::from_u32(2)))),
		(true, ScriptType::Server) => ("Script", Some(Variant::Enum(Enum::from_u32(0)))),
		(true, ScriptType::Client) => ("LocalScript", None),
		(_, ScriptType::Module) => ("ModuleScript", None),
	};

	let mut snapshot = Snapshot::new().with_class(class_name);
	let mut properties = UstrMap::new();

	let source = vfs.read_to_string(path)?;

	if script_type != ScriptType::Module {
		if let Some(run_context) = run_context {
			properties.insert(ustr("RunContext"), run_context);
		}
	}

	properties.insert(ustr("Source"), Variant::String(source));
	snapshot.set_properties(properties);

	Ok(snapshot)
}

#[profiling::function]
pub fn write_luau(mut properties: Properties, path: &Path, vfs: &Vfs) -> Result<Properties> {
	match properties.remove(&ustr("Source")) {
		Some(Variant::String(source)) => vfs.write(path, source.as_bytes())?,
		// Property-only updates must not truncate an existing script. A missing
		// Source means "unchanged", except while creating a brand-new script.
		_ if !vfs.exists(path) => vfs.write(path, &[])?,
		_ => {}
	}

	Ok(properties)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn property_only_update_preserves_existing_source() {
		let vfs = Vfs::new_virtual();
		let path = Path::new("Test.luau");
		vfs.write(path, b"return 42").unwrap();

		let mut properties = Properties::default();
		properties.insert(ustr("Disabled"), Variant::Bool(true));

		let remaining = write_luau(properties, path, &vfs).unwrap();
		assert_eq!(vfs.read_to_string(path).unwrap(), "return 42");
		assert_eq!(remaining.get(&ustr("Disabled")), Some(&Variant::Bool(true)));
	}

	#[test]
	fn explicit_empty_source_clears_existing_script() {
		let vfs = Vfs::new_virtual();
		let path = Path::new("Test.luau");
		vfs.write(path, b"return 42").unwrap();

		let mut properties = Properties::default();
		properties.insert(ustr("Source"), Variant::String(String::new()));

		write_luau(properties, path, &vfs).unwrap();
		assert_eq!(vfs.read_to_string(path).unwrap(), "");
	}

	#[test]
	fn source_less_addition_creates_an_empty_script() {
		let vfs = Vfs::new_virtual();
		let path = Path::new("Test.luau");

		write_luau(Properties::default(), path, &vfs).unwrap();
		assert!(vfs.exists(path));
		assert_eq!(vfs.read_to_string(path).unwrap(), "");
	}
}
