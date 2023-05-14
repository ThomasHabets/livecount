use futures::{pin_mut, select};
use futures_util::FutureExt;
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use std::collections::HashMap;
use std::sync::Arc;
use warp::ws::Message;
use warp::Filter;

use crate::registry::Registry;

pub fn livecount(
    reg: Arc<Registry>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    livecount_index()
        .or(livecount_ws(reg))
        .with(warp::cors().allow_any_origin())
}

pub fn livecount_ws(
    inreg: Arc<Registry>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    warp::path!("livecount" / "ws")
            .and(warp::ws())
            .and(warp::addr::remote())
            .and(warp::header::<String>("origin"))
            .and(warp::header::optional::<String>("x-forwarded-for"))
            .and(warp::filters::header::headers_cloned())
            .and(warp::query::<HashMap<String, String>>())
            .map(move |ws: warp::ws::Ws, remote: Option<std::net::SocketAddr>, origin, orig: Option<String>, _heads, querymap: HashMap<String,String>| {
                let reg = inreg.clone();
                // TODO: smarter x-forwarded-for parsing.
                let remote = match remote {
                    Some(ra) => format!("{:?}", ra),
                    None => "unknown".to_string(),
                };
                ws.on_upgrade(move |websocket| async move {
                    let loc = match querymap.get("l") {
                        Some(v) => v,
                        None => "invalid",
                    };
                    debug!("WS upgrade from {} {} by {}", origin, loc, orig.unwrap_or(remote));
                    if !loc.starts_with(&format!("{}/", origin)) {
                        warn!("ERROR: wrong origin {}, want {}", loc, origin);
                    }
                    let (mut tx, mut rx) = websocket.split();

                    let mut handle = reg.register(loc).await.unwrap();
                    loop {
                        let wsfut = rx.next().fuse();
                        let hnfut = handle.next().fuse();
                        pin_mut!(wsfut, hnfut);
                        // TODO: I don't think this is right. This
                        // discards the previous wait. Maybe it'll be
                        // fine, since aside from closing the
                        // websocket we don't expect anything from it.
                        select! {
                            msg = hnfut => {
                                if let Some(msg) = msg {
                                    if let Err(err) = tx.send(Message::text(format!("{}", msg))).await {
                                        warn!("connection broken?: {:?}", err);
                                        break
                                    }
                                }
                            },
                            wsmsg = wsfut => {
                                debug!("Closing because got something from socket: {:?}", wsmsg);
                                // Ok(Close(Some(CloseFrame { code: Away, reason: "" })))
                                break
                            },
                        };
                    }
                    debug!("WS Terminating");
                    handle.close().await;
                })
            })
}

pub fn livecount_index(
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
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
