use actix_web::{
	post,
	web::{Bytes, Data},
	HttpResponse, Responder,
};
use log::trace;
use std::sync::Arc;

use crate::core::{processor::WriteRequest, Core};

#[post("/write")]
async fn main(body: Bytes, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: write");

	let mut deserializer = rmp_serde::Deserializer::new(body.as_ref());
	let request: WriteRequest = match serde_path_to_error::deserialize(&mut deserializer) {
		Ok(request) => request,
		Err(err) => {
			let message = format!("MsgPack write extraction error at {}: {}", err.path(), err.inner());
			log::error!("{message}");
			return HttpResponse::BadRequest().body(message);
		}
	};

	if !core.queue().is_subscribed(request.client_id) {
		return HttpResponse::Unauthorized().body("Not subscribed");
	}

	match core.processor().write(request) {
		Ok(()) => HttpResponse::Ok().body("Changes durably queued"),
		Err(err) => {
			log::error!("Failed to durably queue Studio changes: {err:#}");
			HttpResponse::InternalServerError().body(format!("Failed to durably queue changes: {err:#}"))
		}
	}
}
