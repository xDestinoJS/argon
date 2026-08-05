use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use log::trace;
use std::sync::Arc;

use crate::core::{processor::WriteRequest, Core};

fn log_ref_properties(name: &str, properties: &crate::Properties, children: &[crate::core::snapshot::Snapshot]) {
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
			println!("ADDED ARGON ID FOR {}, for property '{}' ref to {:?}", name, p, v);
		}
	}
	for child in children {
		log_ref_properties(&child.name, &child.properties, &child.children);
	}
}

#[post("/write")]
async fn main(request: MsgPack<WriteRequest>, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: write");

	let request = request.0;

	for snap in &request.changes.additions {
		log_ref_properties(&snap.name, &snap.properties, &snap.children);
	}

	for snap in &request.changes.updates {
		if let Some(props) = &snap.properties {
			for (p, v) in props {
				let p_str = p.as_str();
				if matches!(v, rbx_dom_weak::types::Variant::Ref(_))
					|| p_str.starts_with("Attachment")
					|| p_str == "PrimaryPart"
					|| p_str == "Part0"
					|| p_str == "Part1"
					|| p_str == "Adornee"
					|| p_str == "Weld"
				{
					println!("ADDED ARGON ID FOR (Update {:?}), for property '{}' ref to {:?}", snap.id, p, v);
				}
			}
		}
	}

	if !core.queue().is_subscribed(request.client_id) {
		return HttpResponse::Unauthorized().body("Not subscribed");
	}

	core.processor().write(request);

	HttpResponse::Ok().body("Written changes successfully")
}
