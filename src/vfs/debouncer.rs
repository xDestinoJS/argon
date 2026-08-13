use crossbeam_channel::Receiver;
use log::trace;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebouncedEvent, Debouncer, FileIdMap};
use std::{
	collections::HashMap,
	fs,
	io::{self, Result},
	path::{Path, PathBuf},
	sync::{mpsc, Arc, Mutex, RwLock},
	thread::Builder,
	time::{Duration, Instant, SystemTime},
};

#[cfg(target_os = "macos")]
use notify::event::DataChange;

#[cfg(not(target_os = "windows"))]
use notify::event::ModifyKind;

#[cfg(target_os = "linux")]
use notify::event::{AccessKind, AccessMode, RenameMode};

use super::VfsEvent;
const EXPECTED_CHANGE_LIFETIME: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathState {
	Missing,
	Directory {
		modified: Option<SystemTime>,
		entries: Vec<PathBuf>,
	},
	File {
		len: u64,
		modified: Option<SystemTime>,
	},
}

#[derive(Clone, Debug)]
struct ExpectedChange {
	state: PathState,
	expires_at: Instant,
}

fn path_state(path: &Path) -> PathState {
	match fs::metadata(path) {
		Ok(metadata) if metadata.is_dir() => {
			let mut entries: Vec<PathBuf> = fs::read_dir(path)
				.map(|entries| {
					entries
						.filter_map(|entry| entry.ok().map(|entry| entry.path()))
						.collect()
				})
				.unwrap_or_default();
			entries.sort();

			PathState::Directory {
				modified: metadata.modified().ok(),
				entries,
			}
		}
		Ok(metadata) => PathState::File {
			len: metadata.len(),
			modified: metadata.modified().ok(),
		},
		Err(_) => PathState::Missing,
	}
}

fn should_ignore_syncback_event(is_paused: bool, resumed_at: Instant, event_time: Instant) -> bool {
	// Only discard events that actually happened while the VFS was paused.
	// The old post-resume grace period also discarded legitimate editor writes.
	is_paused || event_time <= resumed_at
}

fn matches_expected_change(expected: &mut HashMap<PathBuf, ExpectedChange>, path: &Path, now: Instant) -> bool {
	expected.retain(|_, change| change.expires_at > now);

	let Some(change) = expected.get(path) else {
		return false;
	};

	if path_state(path) == change.state {
		// Keep the expectation until expiry because Windows commonly emits more
		// than one notification for a single filesystem operation.
		true
	} else {
		expected.remove(path);
		false
	}
}

#[cfg(target_os = "linux")]
const DEBOUNCE_TIME: Duration = Duration::from_micros(500);

macro_rules! event_path {
	($event:expr) => {
		$event.paths.first().unwrap().to_owned()
	};
}

#[cfg(target_os = "linux")]
struct DebounceContext {
	time: Instant,
	path: PathBuf,
}

pub struct VfsDebouncer {
	inner: Debouncer<RecommendedWatcher, FileIdMap>,
	pause_state: Arc<RwLock<(bool, Instant)>>,
	expected_changes: Arc<Mutex<HashMap<PathBuf, ExpectedChange>>>,
	watched_roots: Vec<PathBuf>,
	receiver: Receiver<VfsEvent>,
}

impl VfsDebouncer {
	pub fn new() -> Self {
		let (inner_sender, inner_receiver) = mpsc::channel();
		let (sender, receiver) = crossbeam_channel::unbounded();

		let debouncer = new_debouncer(Duration::from_millis(100), None, inner_sender, false).unwrap();

		let pause_state = Arc::new(RwLock::new((false, Instant::now())));
		let local_pause_state = pause_state.clone();
		let expected_changes = Arc::new(Mutex::new(HashMap::new()));
		let local_expected_changes = expected_changes.clone();

		Builder::new()
			.name("debouncer".into())
			.spawn(move || {
				#[cfg(target_os = "linux")]
				let mut context = DebounceContext {
					time: Instant::now(),
					path: PathBuf::new(),
				};

				for events in inner_receiver {
					for event in events.unwrap() {
						let (is_paused, timestamp) = *local_pause_state.read().unwrap();

						// Use the time the filesystem event occurred, not the time its
						// debounced batch happened to arrive. This reliably rejects delayed
						// client-syncback echoes without hiding later server-side edits.
						let path = event_path!(event);
						if should_ignore_syncback_event(is_paused, timestamp, event.time)
							|| matches_expected_change(
								&mut local_expected_changes.lock().unwrap(),
								&path,
								Instant::now(),
							) {
							continue;
						}

						trace!("Debouncing event, paths: {:?}, kind: {:?}", event.paths, event.kind);

						#[cfg(not(target_os = "linux"))]
						if let Some(event) = debounce(&event) {
							sender.send(event).unwrap();
						}

						#[cfg(target_os = "linux")]
						if let Some(event) = debounce(&event, &mut context) {
							sender.send(event).unwrap();
						}
					}
				}
			})
			.unwrap();

		Self {
			inner: debouncer,
			pause_state,
			expected_changes,
			watched_roots: Vec::new(),
			receiver,
		}
	}

	pub fn watch(&mut self, path: &Path, recursive: bool) -> Result<()> {
		let recursive = if recursive {
			RecursiveMode::Recursive
		} else {
			RecursiveMode::NonRecursive
		};

		self.inner.watcher().watch(path, recursive).map_err(map_error)?;
		self.inner.cache().add_root(path, recursive);
		self.watched_roots.push(path.to_owned());

		Ok(())
	}

	pub fn unwatch(&mut self, path: &Path) -> Result<()> {
		self.inner.watcher().unwatch(path).map_err(map_error)?;
		self.inner.cache().remove_root(path);
		self.watched_roots.retain(|root| root != path);

		Ok(())
	}

	pub fn pause(&mut self) {
		*self.pause_state.write().unwrap() = (true, Instant::now());
	}

	pub fn resume(&mut self) {
		*self.pause_state.write().unwrap() = (false, Instant::now());
	}

	/// Record the final state of a path changed by Studio syncback. Delayed
	/// watcher notifications are ignored only while that exact state remains;
	/// a real disk edit changes the fingerprint and is processed immediately.
	pub fn record_syncback_path(&mut self, path: &Path) {
		let mut expected = self.expected_changes.lock().unwrap();
		let expires_at = Instant::now() + EXPECTED_CHANGE_LIFETIME;

		let watched_root = self
			.watched_roots
			.iter()
			.filter(|root| path.starts_with(root))
			.max_by_key(|root| root.components().count())
			.cloned();

		// Windows reports both the changed path and one or more containing
		// directories. Record the entire affected chain. Directory fingerprints
		// include their entries, so a genuine user move is still processed.
		for affected_path in path.ancestors() {
			expected.insert(
				affected_path.to_owned(),
				ExpectedChange {
					state: path_state(affected_path),
					expires_at,
				},
			);

			if watched_root.as_deref() == Some(affected_path) || watched_root.is_none() {
				break;
			}
		}
	}

	pub fn receiver(&self) -> Receiver<VfsEvent> {
		self.receiver.clone()
	}
}

fn map_error(err: notify::Error) -> io::Error {
	match err.kind {
		notify::ErrorKind::Io(err) => err,
		notify::ErrorKind::PathNotFound => io::Error::new(io::ErrorKind::NotFound, err),
		notify::ErrorKind::WatchNotFound => io::Error::new(io::ErrorKind::NotFound, err),
		_ => io::Error::other(err),
	}
}

#[cfg(target_os = "macos")]
fn debounce(event: &DebouncedEvent) -> Option<VfsEvent> {
	match event.kind {
		EventKind::Create(_) => {
			let path = event_path!(event);

			if path.exists() {
				Some(VfsEvent::Create(path))
			} else {
				None
			}
		}
		EventKind::Remove(_) => Some(VfsEvent::Delete(event_path!(event))),
		EventKind::Modify(kind) => match kind {
			ModifyKind::Name(_) => {
				let path = event_path!(event);

				if path.exists() {
					Some(VfsEvent::Create(path))
				} else {
					Some(VfsEvent::Delete(path))
				}
			}
			ModifyKind::Data(kind) => {
				if kind == DataChange::Content {
					Some(VfsEvent::Write(event_path!(event)))
				} else {
					None
				}
			}
			_ => None,
		},
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn delayed_events_from_a_paused_syncback_are_ignored() {
		let resumed_at = Instant::now();
		let event_time = resumed_at - Duration::from_secs(1);

		assert!(should_ignore_syncback_event(false, resumed_at, event_time));
	}

	#[test]
	fn later_server_events_are_not_ignored() {
		let resumed_at = Instant::now();
		let event_time = resumed_at + Duration::from_millis(1);

		assert!(!should_ignore_syncback_event(false, resumed_at, event_time));
	}

	#[test]
	fn expected_events_are_path_and_state_specific() {
		let root = std::env::temp_dir().join(format!("argon-debouncer-{}", uuid::Uuid::new_v4()));
		fs::create_dir_all(&root).unwrap();
		let path = root.join("script.luau");
		fs::write(&path, "first").unwrap();

		let mut expected = HashMap::new();
		expected.insert(
			path.clone(),
			ExpectedChange {
				state: path_state(&path),
				expires_at: Instant::now() + EXPECTED_CHANGE_LIFETIME,
			},
		);

		assert!(matches_expected_change(&mut expected, &path, Instant::now()));
		std::thread::sleep(Duration::from_millis(2));
		fs::write(&path, "second and different").unwrap();
		assert!(!matches_expected_change(&mut expected, &path, Instant::now()));

		fs::remove_dir_all(root).unwrap();
	}
}

#[cfg(target_os = "linux")]
fn debounce(event: &DebouncedEvent, context: &mut DebounceContext) -> Option<VfsEvent> {
	match event.kind {
		EventKind::Create(_) => {
			let path = event_path!(event);

			context.time = event.time;
			context.path.clone_from(&path);

			Some(VfsEvent::Create(path))
		}
		EventKind::Remove(_) => Some(VfsEvent::Delete(event_path!(event))),
		EventKind::Modify(ModifyKind::Name(mode)) => match mode {
			RenameMode::From => Some(VfsEvent::Delete(event_path!(event))),
			RenameMode::To => Some(VfsEvent::Create(event_path!(event))),
			_ => None,
		},
		EventKind::Access(kind) => {
			if kind == AccessKind::Close(AccessMode::Write) {
				let duration = event.time.duration_since(context.time);
				let path = event_path!(event);

				if duration < DEBOUNCE_TIME && path == context.path {
					return None;
				}

				Some(VfsEvent::Write(path))
			} else {
				None
			}
		}
		_ => None,
	}
}

#[cfg(target_os = "windows")]
fn debounce(event: &DebouncedEvent) -> Option<VfsEvent> {
	match event.kind {
		EventKind::Create(_) => Some(VfsEvent::Create(event_path!(event))),
		EventKind::Remove(_) => Some(VfsEvent::Delete(event_path!(event))),
		EventKind::Modify(_) => Some(VfsEvent::Write(event_path!(event))),
		_ => None,
	}
}
