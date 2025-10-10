use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use tokio::time::Duration;
use warp::http::header::HeaderMap;
use warp::ws::{Message, WebSocket};
use warp::Filter;
use warp::Reply;

use crate::registry::Registry;
use crate::registry::{TIMEOUTS, UPDATES_SENT, WS_RX_TYPE};

mod static_assert {
    /// This asserts that ping life is at least 30 seconds before end of
    /// websocket life.
    const _MUST_WORK: u64 = super::MAX_WS_LIFE_PING_SECS - 30;
}

/// Max lifetime of an idle websocket.
const MAX_WS_LIFE_SECS: u64 = 600; // 10 minutes.
const MAX_WS_LIFE: Duration = Duration::from_secs(MAX_WS_LIFE_SECS);

/// Websocket lifetime after which a ping is sent.
const MAX_WS_LIFE_PING_SECS: u64 = MAX_WS_LIFE_SECS - 60;
const MAX_WS_LIFE_PING: Duration = Duration::from_secs(MAX_WS_LIFE_PING_SECS);

/// Timeout for sending websocket message.
const MAX_WS_SEND_TIME: Duration = Duration::from_secs(5);

pub fn livecount(
    reg: Arc<Registry>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    debug!("livecount()");
    livecount_index()
        .or(livecount_ws(reg))
        .with(warp::cors().allow_any_origin())
}

/// Send a message on a websocket, with a timeout.
///
/// On error, return a one-word string suitable for putting in the prometheus
/// metric.
async fn websocket_send(
    tx: &mut SplitSink<WebSocket, Message>,
    msg: Message,
) -> std::result::Result<(), String> {
    let timeout = tokio::time::sleep(MAX_WS_SEND_TIME);
    tokio::pin!(timeout);
    tokio::select! {
        _ = timeout => {
            Err("timeout".to_string())
        },
        r = tx.send(msg) => {
            match r {
                Ok(_) => Ok(()),
                Err(e) => {
                    info!("Failed to send websocket message: {e}");
                    Err(e.to_string())
                }
            }
        }
    }
}

async fn livecount_ws_map_upgrade(
    websocket: WebSocket,
    remote: String,
    url: &url::Url,
    reg: Arc<Registry>,
) {
    debug!("livecount_ws_map_upgrade()");
    debug!("WS upgrade on {url} by {remote}");

    // Split the websocket so that we can give it to separate futures.
    let (mut tx, mut rx) = websocket.split();

    // See https://biriukov.dev/docs/async-rust-tokio-io/3-tokio-io-patterns/ pattern.
    let mut handle = reg.register(url.as_str()).await.unwrap();

    // Sleep until connection times out. Gets reset on every incoming activity.
    let new_sleep: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>> = Default::default();
    let new_sleep_renew = || async {
        (*new_sleep.lock().await) = Some(tokio::time::Instant::now() + MAX_WS_LIFE);
    };
    new_sleep_renew().await;

    // Sleep until it's time to send a ping. Also gets reset on activity.
    let new_sleep_ping: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>> = Default::default();
    let new_sleep_ping_renew = || async {
        (*new_sleep_ping.lock().await) = Some(tokio::time::Instant::now() + MAX_WS_LIFE_PING);
    };
    new_sleep_ping_renew().await;

    // TODO: why 10?
    let (to_client_tx, mut to_client_rx) = tokio::sync::mpsc::channel(10);

    // Async that gets updates from the registry.
    let from_registry = async {
        while let Some(msg) = handle.next().await {
            if let Err(e) = to_client_tx.send(Message::text(format!("{msg}"))).await {
                error!("Failed to send to client-sender: {e:?}");
                return Err("failed to send to client-sender".to_owned());
            }
        }
        debug!("Registry closing");
        Ok(())
    };

    // Async that actually sends on websocket.
    let to_client = async {
        while let Some(msg) = to_client_rx.recv().await {
            match websocket_send(&mut tx, msg).await {
                Err(e) => {
                    warn!("Error sending on websocket: {e}");
                    UPDATES_SENT.with_label_values(&["data", &e]).inc();
                    return Err("sending on websocket".to_owned());
                }
                Ok(_) => {
                    UPDATES_SENT.with_label_values(&["data", "ok"]).inc();
                }
            }
        }
        debug!("Nothing left to send to client");
        Ok(())
    };

    // Async that reads from client.
    let from_client = async {
        loop {
            let wsmsg = rx.next().await;
            match wsmsg {
                None => {
                    debug!("Got None message, disconnecting");
                    WS_RX_TYPE.with_label_values(&["none"]).inc();
                    return Err::<(), _>("got None message".to_owned());
                }
                Some(Ok(ref m)) => {
                    debug!("Got a message: {m:?}");
                    new_sleep_ping_renew().await;
                    if m.is_close() {
                        debug!("WS Disconnection: {:?}", wsmsg);
                        WS_RX_TYPE.with_label_values(&["away"]).inc();
                        return Err("WS disconnection".to_owned());
                    } else if m.is_ping() {
                        WS_RX_TYPE.with_label_values(&["ping"]).inc();
                        new_sleep_renew().await;
                    } else if m.is_text() {
                        WS_RX_TYPE.with_label_values(&["text"]).inc();
                        new_sleep_renew().await;
                    } else if m.is_binary() {
                        WS_RX_TYPE.with_label_values(&["binary"]).inc();
                        new_sleep_renew().await;
                    } else if m.is_pong() {
                        WS_RX_TYPE.with_label_values(&["pong"]).inc();
                        new_sleep_renew().await;
                    } else {
                        WS_RX_TYPE.with_label_values(&["unknown"]).inc();
                        error!("Unknown message type: {wsmsg:?}");
                    }
                }
                Some(Err(e)) => {
                    WS_RX_TYPE.with_label_values(&[format!("{e}")]).inc();
                    error!("Error receiving message? {e}");
                    return Err("error receiving message".to_owned());
                }
            };
        }
    };

    // Async that triggers a total timeout since last activity.
    // Any async returning Err will terminate all of them.
    let f_timeout = async {
        // Sleep as long as deadline keeps being updated.
        loop {
            let deadline = {
                let mut s = new_sleep.lock().await;
                match s.take() {
                    None => break,
                    Some(news) => news,
                }
            };
            tokio::time::sleep_until(deadline).await;
        }
        debug!("Max websocket time exceeded");
        TIMEOUTS.with_label_values(&["final"]).inc();
        Err::<(), _>("timeout".to_owned())
    };

    // Async that triggers sending a ping.
    let f_timeout_ping = async {
        loop {
            // Sleep as long as the deadline keeps getting updated.
            loop {
                let deadline = {
                    let mut s = new_sleep_ping.lock().await;
                    match s.take() {
                        None => break,
                        Some(news) => news,
                    }
                };
                tokio::time::sleep_until(deadline).await;
            }

            // Send a ping.
            debug!("Max websocket ping time exceeded. Sending ping.");
            TIMEOUTS.with_label_values(&["ping"]).inc();
            match to_client_tx
                .send(Message::ping("livecount".as_bytes()))
                .await
            {
                Err(e) => {
                    warn!("Error sending ping on websocket: {e}");
                    UPDATES_SENT
                        .with_label_values(&["ping", &format!("{e:?}")])
                        .inc();
                    return Err::<(), _>("error sending on websocket".to_owned());
                }
                Ok(_) => {
                    UPDATES_SENT.with_label_values(&["ping", "ok"]).inc();
                }
            }

            // We may not need another ping; if the first one is not replied to, why would a second
            // one? But we don't want to busyloop nor exit the async.
            new_sleep_ping_renew().await;
        }
    };

    // Run all asyncs. If any of them return error, terminate them all.
    if let Err(e) = tokio::try_join!(
        from_registry,
        to_client,
        from_client,
        f_timeout,
        f_timeout_ping
    ) {
        debug!("WS asyncs ended with: {e:?}");
    }

    debug!("WS Terminating");
    handle.close().await;
}

fn livecount_ws_map(
    ws: warp::ws::Ws,
    remote: Option<std::net::SocketAddr>,
    origin: Option<String>,
    _heads: HeaderMap,
    querymap: HashMap<String, String>,
    inreg: Arc<Registry>,
) -> impl Reply {
    debug!("livecount_ws_map()");
    let reg = inreg.clone();
    // TODO: smarter x-forwarded-for parsing.
    let remote = match remote {
        Some(ra) => format!("{:?}", ra),
        None => "unknown".to_string(),
    };
    let Some(Ok(mut url)) = querymap.get("l").map(|s| url::Url::parse(s)) else {
        panic!();
    };
    url.set_query(None);

    {
        let want = format!("{}/", origin.clone().unwrap_or_default());
        if !url.as_str().starts_with(&want) {
            warn!("ERROR: wrong origin {url}, want starts with {origin:?}");
        }
    }
    ws.on_upgrade(move |websocket| async move {
        livecount_ws_map_upgrade(websocket, remote, &url, reg).await;
    })
}

fn livecount_ws(
    inreg: Arc<Registry>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    debug!("livecount_ws()");
    warp::path!("livecount" / "ws")
        .and(warp::ws())
        .and(warp::addr::remote())
        .and(warp::header::optional::<String>("origin"))
        .and(warp::filters::header::headers_cloned())
        .and(warp::query::<HashMap<String, String>>())
        .map(
            move |ws: warp::ws::Ws,
                  remote: Option<std::net::SocketAddr>,
                  origin: Option<String>,
                  heads,
                  querymap: HashMap<String, String>| {
                livecount_ws_map(ws, remote, origin, heads, querymap, inreg.clone())
            },
        )
}

fn livecount_index() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
{
    debug!("livecount_index()");
    warp::path!("livecount" / "health")
        .and(warp::get())
        .map(|| {
            warp::reply::html(
                r#"
<html>
<head>
</head>
<body>
Live counter: <span id="counter"></div>
</body>
<script>
var socket;
function reconnect() {
  let loc = window.location;
  let scheme = "wss";
  if (loc.protocol == "http:") {
    scheme = "ws";
  }
  let path = `${scheme}://${loc.host}/livecount/ws?l=${loc}`;
  socket = new WebSocket(path);
  socket.addEventListener('open', (event) => {
    console.log('ws opened');
  });
  socket.addEventListener('close', (event) => {
      console.log('ws closed');
      reconnect();
  });
  socket.addEventListener('message', (event) => {
    console.log('Message from server ', event.data);
    document.getElementById("counter").innerHTML = event.data;
  });
}
reconnect();
//setInterval(() => {  socket.ping();}, 10000);
</script>
</html>
"#,
            )
        })
}
