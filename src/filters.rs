use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, trace, warn};
use tokio::time::Duration;
use warp::http::header::HeaderMap;
use warp::ws::{Message, WebSocket};
use warp::Filter;
use warp::Reply;

use crate::registry::Registry;
use crate::registry::{UPDATES_SENT, WS_RX_TYPE};

const MAX_WS_LIFE: Duration = Duration::from_secs(600);
const MAX_WS_SEND_TIME: Duration = Duration::from_secs(5);

pub fn livecount(
    reg: Arc<Registry>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    debug!("livecount()");
    livecount_index()
        .or(livecount_ws(reg))
        .with(warp::cors().allow_any_origin())
}

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
                Ok(_) => {
                    trace!("Sent websocket message");
                    Ok(())
                },
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
    origin: String,
    orig: Option<String>,
    querymap: HashMap<String, String>,
    reg: Arc<Registry>,
) {
    debug!("livecount_ws_map_upgrade()");
    let loc = match querymap.get("l") {
        Some(v) => v,
        None => "invalid",
    };
    let loc = match loc.find('?') {
        Some(pos) => &loc[..pos],
        None => loc,
    };
    debug!(
        "WS upgrade from {} {} by {}",
        origin,
        loc,
        orig.unwrap_or(remote)
    );
    if !loc.starts_with(&format!("{}/", origin)) {
        warn!("ERROR: wrong origin {}, want {}", loc, origin);
    }
    let (mut tx, mut rx) = websocket.split();

    let mut handle = reg.register(loc).await.unwrap();
    {
        let deadline = tokio::time::Instant::now() + MAX_WS_LIFE;
        let timeout = tokio::time::sleep_until(deadline);
        tokio::pin!(timeout);
        //let wsfut = rx.next().fuse();
        loop {
            //let hnfut = handle.next().fuse();

            //pin_mut!(wsfut, hnfut);
            // TODO: I don't think this is right. This
            // discards the previous wait. Maybe it'll be
            // fine, since aside from closing the
            // websocket we don't expect anything from it.
            let mut new_sleep = None;
            tokio::select! {
                msg = handle.next() => {
                    if let Some(msg) = msg {
                        match websocket_send(&mut tx, Message::text(format!("{msg}"))).await {
                            Err(e) => {
                                warn!("Error sending on websocket: {e}");
                                UPDATES_SENT.with_label_values(&[e]).inc();
                                break;
                            },
                            Ok(_) => {
                                UPDATES_SENT.with_label_values(&["ok"]).inc();
                            },
                        }
                    }
                },
                wsmsg = rx.next() => {
                    match wsmsg {
                        None => {
                            debug!("Got None message, disconnecting");
                            WS_RX_TYPE.with_label_values(&["none"]).inc();
                            break;
                        },
                        Some(Ok(ref m)) => {
                            debug!("Got a message: {m:?}");
                            if m.is_close(){
                                debug!("WS Disconnection: {:?}", wsmsg);
                                WS_RX_TYPE.with_label_values(&["away"]).inc();
                                break;
                            } else if m.is_ping() {
                                WS_RX_TYPE.with_label_values(&["ping"]).inc();
                                new_sleep = Some(MAX_WS_LIFE);
                            } else if m.is_text() {
                                WS_RX_TYPE.with_label_values(&["text"]).inc();
                            } else if m.is_binary() {
                                WS_RX_TYPE.with_label_values(&["binary"]).inc();
                            } else if m.is_pong() {
                                WS_RX_TYPE.with_label_values(&["pong"]).inc();
                                new_sleep = Some(MAX_WS_LIFE);
                            } else {
                                WS_RX_TYPE.with_label_values(&["unknown"]).inc();
                                error!("Unknown message type: {wsmsg:?}");
                            }
                        },
                        Some(Err(e)) => {
                            WS_RX_TYPE.with_label_values(&["error"]).inc();
                            error!("Error receiving message? {e}");
                            break;
                        },
                    };
                },
                _ = &mut timeout => {
                    debug!("Max websocket time exceeded");
                    crate::registry::TIMEOUTS.inc();
                    break;
                }
            };
            if let Some(t) = new_sleep {
                timeout.as_mut().reset(tokio::time::Instant::now() + t);
            }
        }
    }
    debug!("WS Terminating");
    handle.close().await;
}

fn livecount_ws_map(
    ws: warp::ws::Ws,
    remote: Option<std::net::SocketAddr>,
    origin: String,
    orig: Option<String>,
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
    ws.on_upgrade(move |websocket| async move {
        livecount_ws_map_upgrade(websocket, remote, origin, orig, querymap, reg).await;
    })
}

fn livecount_ws(
    inreg: Arc<Registry>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    debug!("livecount_ws()");
    warp::path!("livecount" / "ws")
        .and(warp::ws())
        .and(warp::addr::remote())
        .and(warp::header::<String>("origin"))
        .and(warp::header::optional::<String>("x-forwarded-for"))
        .and(warp::filters::header::headers_cloned())
        .and(warp::query::<HashMap<String, String>>())
        .map(
            move |ws: warp::ws::Ws,
                  remote: Option<std::net::SocketAddr>,
                  origin,
                  orig: Option<String>,
                  heads,
                  querymap: HashMap<String, String>| {
                livecount_ws_map(ws, remote, origin, orig, heads, querymap, inreg.clone())
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
