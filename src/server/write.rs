use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use log::trace;
use std::sync::Arc;

use crate::core::{processor::WriteRequest, Core};

fn log_snapshot_tree(name: &str, class: &str, properties: &crate::Properties, children: &[crate::core::snapshot::Snapshot], depth: usize) {
	let indent = "  ".repeat(depth);
	let keys: Vec<_> = properties.keys().map(|k| k.as_str()).collect();
	println!("{}[RUST RECV ADD] Instance: '{}' ({}), Props count: {}, Keys: {:?}", indent, name, class, keys.len(), keys);
	for (p, v) in properties {
		let p_str = p.as_str();
		if matches!(v, rbx_dom_weak::types::Variant::Ref(_))
			|| p_str.starts_with("Attachment")
			|| p_str == "PrimaryPart"
			|| p_str == "Part0"
			|| p_str == "Part1"
			|| p_str == "Adornee"
			|| p_str == "Weld"
		{
			println!("{}   -> [REF PROP] {} = {:?}", indent, p, v);
		}
	}
	for child in children {
		log_snapshot_tree(&child.name, child.class.as_str(), &child.properties, &child.children, depth + 1);
	}
}

#[post("/write")]
async fn main(request: MsgPack<WriteRequest>, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: write");

	let request = request.0;

	println!(
		"[RUST /write RECEIVE] Client ID: {}, Additions: {}, Updates: {}, Removals: {}",
		request.client_id,
		request.changes.additions.len(),
		request.changes.updates.len(),
		request.changes.removals.len()
	);

	for snap in &request.changes.additions {
		log_snapshot_tree(&snap.name, snap.class.as_str(), &snap.properties, &snap.children, 0);
	}

	for snap in &request.changes.updates {
		let keys = snap.properties.as_ref().map(|p| p.keys().map(|k| k.as_str()).collect::<Vec<_>>());
		println!("[RUST RECV UPDATE] ID: {:?}, Keys: {:?}", snap.id, keys);
		if let Some(props) = &snap.properties {
			for (p, v) in props {
				println!("   -> [RUST UPDATE PROP] {} = {:?}", p, v);
			}
		}
	}

	if !core.queue().is_subscribed(request.client_id) {
		println!("[RUST /write REJECTED] Client ID {} is NOT subscribed!", request.client_id);
		return HttpResponse::Unauthorized().body("Not subscribed");
	}

	core.processor().write(request);

	HttpResponse::Ok().body("Written changes successfully")
}
