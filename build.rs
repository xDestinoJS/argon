use anyhow::{Context, Result};
use self_update::backends::github::Update;
use std::{env, fs, path::PathBuf};

fn main() -> Result<()> {
	let out_path = PathBuf::from(env::var("OUT_DIR")?).join("Argon.rbxm");
	println!("cargo:rerun-if-env-changed=ARGON_PLUGIN_PATH");

	if !cfg!(feature = "plugin") {
		fs::File::create(out_path)?;
		return Ok(());
	}

	if let Ok(plugin_path) = env::var("ARGON_PLUGIN_PATH") {
		fs::copy(&plugin_path, &out_path)
			.with_context(|| format!("Failed to bundle local Argon plugin from {plugin_path}"))?;
		return Ok(());
	}

	let mut builder = Update::configure();

	if let Ok(token) = env::var("GITHUB_TOKEN") {
		builder.auth_token(&token);
	} else {
		println!("cargo:warning=GITHUB_TOKEN not set, rate limits may apply!")
	}

	builder
		.repo_owner("argon-rbx")
		.repo_name("argon-roblox")
		.bin_name("Argon.rbxm")
		.bin_install_path(out_path)
		.target("");

	builder
		.build()?
		.download()
		.context("Failed to download Argon plugin from GitHub!")?;

	Ok(())
}
