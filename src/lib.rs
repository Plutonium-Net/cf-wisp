use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use futures_util::lock::Mutex;
use regex::Regex;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use wisp_mux::{
	packet::{CloseReason, ConnectPacket, StreamType},
	stream::MuxStream,
	ws::TransportWrite,
	ServerMux, WispError,
};
use worker::{
	console_log, console_warn, event, wasm_bindgen_futures::spawn_local,
	worker_sys::web_sys::WebSocket, Context, Env, Error, Headers, Request, Response, ResponseBody,
	Result, Socket, WebSocketPair,
};
use ws_wrapper::WebSocketWrapper;
mod ws_wrapper;

/// Health counters. Cloudflare runs many isolates and evicts them freely, so
/// these describe the isolate answering the request, not the whole Worker.
static WS_ACTIVE: AtomicU64 = AtomicU64::new(0);
static WS_TOTAL: AtomicU64 = AtomicU64::new(0);
static STREAM_ACTIVE: AtomicU64 = AtomicU64::new(0);
static STREAM_TOTAL: AtomicU64 = AtomicU64::new(0);
static FIRST_REQUEST_MS: OnceLock<f64> = OnceLock::new();

const MAX_STREAMS_PER_CONNECTION: usize = 6;

fn now_ms() -> f64 {
	worker::js_sys::Date::now()
}

fn to_worker(val: WispError) -> Error {
	Error::RustError(val.to_string())
}

fn uptime() -> String {
	let started = *FIRST_REQUEST_MS.get_or_init(now_ms);
	let secs = ((now_ms() - started) / 1000.0).max(0.0) as u64;
	match secs {
		0..=59 => format!("{secs}s"),
		60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
		_ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
	}
}

fn health(req: &Request, env: &Env) -> Result<Response> {
	let host = req
		.url()
		.ok()
		.and_then(|url| url.host_str().map(str::to_owned))
		.unwrap_or_else(|| "unknown".to_owned());

	let host_filter = match env.var("allowed_host_regex") {
		Ok(value) => format!("enabled ({value})"),
		Err(_) => "disabled (no allowed_host_regex set)".to_owned(),
	};

	let lines = [
		"CF-WispJS running".to_owned(),
		String::new(),
		format!("endpoint:    wss://{host}/"),
		"protocol:    Wisp v1/v2 over WebSocket".to_owned(),
		format!("uptime:      {} (since this isolate's first request)", uptime()),
		format!(
			"sessions:    {} active, {} total",
			WS_ACTIVE.load(Ordering::Relaxed),
			WS_TOTAL.load(Ordering::Relaxed)
		),
		format!(
			"tcp streams: {} active, {} total",
			STREAM_ACTIVE.load(Ordering::Relaxed),
			STREAM_TOTAL.load(Ordering::Relaxed)
		),
		format!("host filter: {host_filter}"),
		format!(
			"limits:      TCP only, {MAX_STREAMS_PER_CONNECTION} streams per connection, 1 per host:port"
		),
		"scope:       this isolate only (counters reset on cold start)".to_owned(),
	];
	let body = lines.join("\n") + "\n";

	Ok(
		Response::from_body(ResponseBody::Body(body.into_bytes()))?.with_headers(
			Headers::from_iter(
				[
					("Content-Type", "text/plain; charset=utf-8"),
					("Cache-Control", "no-store"),
				]
				.into_iter(),
			),
		),
	)
}

async fn stream_handler(
	packet: ConnectPacket,
	stream: MuxStream<impl TransportWrite>,
) -> Result<()> {
	match packet.stream_type {
		StreamType::Tcp => {
			let mut socket = Socket::builder().connect(packet.host, packet.port)?;
			let mut wisp_socket = stream.into_async_rw().compat();
			tokio::io::copy_bidirectional(&mut socket, &mut wisp_socket).await?;
		}
		StreamType::Udp | StreamType::Other(_) => stream
			.close(CloseReason::ServerStreamInvalidInfo)
			.await
			.map_err(to_worker)?,
	}
	Ok(())
}

async fn ws_handler(ws: WebSocket, env: Env) -> Result<()> {
	let (tx, rx) = WebSocketWrapper::connect(ws, || {
		WS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
	});

	console_log!("Accepted websocket");

	let (server, fut) = ServerMux::new(rx.into_read(), tx.into_write(), 128, None)
		.await
		.map_err(to_worker)?
		.with_no_required_extensions();

	console_log!("Created mux");

	spawn_local(async move {
		console_warn!("Mux future result: {:?}", fut.await);
	});

	let connected_vec = Arc::new(Mutex::new(Vec::new()));

	let allowed_host_regex = env.var("allowed_host_regex").ok().and_then(|x| {
		let regex = Regex::try_from(x.to_string()).ok();
		if regex.is_none() {
			console_warn!("allowed_host_regex was invalid. continuing without allowed_host_regex");
		}
		regex
	});

	while let Some((packet, stream)) = server.wait_for_stream().await {
		let connected_vec = connected_vec.clone();
		let stream_ident = (packet.host.clone(), packet.port);
		let mut locked = connected_vec.lock().await;
		if locked.contains(&stream_ident) || locked.len() >= MAX_STREAMS_PER_CONNECTION {
			stream
				.close(CloseReason::ServerStreamThrottled)
				.await
				.map_err(to_worker)?;
			console_warn!("Blocked stream creation because one stream for that combo was already created or too many sockets were created");
			continue;
		}
		if let Some(ref regex) = allowed_host_regex {
			if !regex.is_match(&packet.host) {
				stream
					.close(CloseReason::ServerStreamBlockedAddress)
					.await
					.map_err(to_worker)?;
			}
		}
		locked.push(stream_ident.clone());
		drop(locked);
		STREAM_ACTIVE.fetch_add(1, Ordering::Relaxed);
		STREAM_TOTAL.fetch_add(1, Ordering::Relaxed);
		console_log!("Created stream");
		spawn_local(async move {
			console_warn!(
				"Stream handle future result: {:?}",
				stream_handler(packet, stream).await
			);
			STREAM_ACTIVE.fetch_sub(1, Ordering::Relaxed);
			connected_vec.lock().await.retain(|x| *x != stream_ident);
		})
	}

	Ok(())
}

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
	if req.headers().get("Upgrade")?.unwrap_or_default() != "websocket" {
		return health(&req, &env);
	}

	let pair = WebSocketPair::new()?;
	pair.server.accept()?;
	let server = pair.server;

	WS_ACTIVE.fetch_add(1, Ordering::Relaxed);
	WS_TOTAL.fetch_add(1, Ordering::Relaxed);

	spawn_local(async move {
		let _ = ws_handler(server.as_ref().clone(), env).await;
	});

	Response::from_websocket(pair.client)
}
