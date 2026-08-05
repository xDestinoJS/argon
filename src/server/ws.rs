use actix_web::{get, web::Data, HttpRequest, HttpResponse, Error};
use actix_ws::AggregatedMessage;
use futures_util::stream::StreamExt;
use log::trace;
use std::sync::Arc;

use crate::{
	core::{processor::WriteRequest, Core},
};

#[get("/ws")]
pub async fn main(req: HttpRequest, stream: actix_web::web::Payload, core: Data<Arc<Core>>) -> Result<HttpResponse, Error> {
	trace!("Received request: ws");

	let (res, mut session, stream) = actix_ws::handle(&req, stream)?;

	let client_id = (uuid::Uuid::new_v4().as_u128() & 0x7FFFFFFF) as u32;
	let _ = core.queue().subscribe(client_id, "Studio WebSocket");

	let core_send = core.get_ref().clone();
	let mut session_send = session.clone();

	// Task: Push server changes -> WebSocket
	actix_web::rt::spawn(async move {
		loop {
			let core_task = core_send.clone();
			let msg_res = actix_web::rt::task::spawn_blocking(move || {
				core_task.queue().get_timeout(client_id)
			})
			.await;

			match msg_res {
				Ok(Ok(message)) => {
					if let Ok(bytes) = rmp_serde::to_vec(&message) {
						if session_send.binary(bytes).await.is_err() {
							break;
						}
					}
				}
				_ => {
					if !core_send.queue().is_subscribed(client_id) {
						break;
					}
				}
			}
		}
		let _ = core_send.queue().unsubscribe(client_id);
	});

	// Task: Receive Studio changes -> Server
	let core_recv = core.get_ref().clone();
	actix_web::rt::spawn(async move {
		let mut stream = stream.aggregate_continuations();
		while let Some(Ok(msg)) = stream.next().await {
			match msg {
				AggregatedMessage::Binary(bytes) => {
					if let Ok(mut write_req) = rmp_serde::from_slice::<WriteRequest>(&bytes) {
						write_req.client_id = client_id;
						core_recv.processor().write(write_req);
					}
				}
				AggregatedMessage::Text(text) => {
					if let Ok(mut write_req) = serde_json::from_str::<WriteRequest>(&text) {
						write_req.client_id = client_id;
						core_recv.processor().write(write_req);
					}
				}
				AggregatedMessage::Ping(bytes) => {
					let _ = session.pong(&bytes).await;
				}
				AggregatedMessage::Close(_) => {
					break;
				}
				_ => {}
			}
		}
		let _ = core_recv.queue().unsubscribe(client_id);
	});

	Ok(res)
}
