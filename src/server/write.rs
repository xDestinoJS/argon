use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use log::trace;
use std::sync::Arc;

use crate::core::{processor::WriteRequest, Core};

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
		let keys: Vec<_> = snap.properties.keys().map(|k| k.as_str()).collect();
		println!("[RUST RECV ADD] Instance: '{}' ({}), Properties count: {}, Keys: {:?}", snap.name, snap.class, keys.len(), keys);
		for (p, v) in &snap.properties {
			println!("   -> [RUST ADD PROP] {} = {:?}", p, v);
		}
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
