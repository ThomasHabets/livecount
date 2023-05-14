use lazy_static::lazy_static;
use log::{debug, warn};
use prometheus::{IntGauge, Registry as PromReg};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendError;

lazy_static! {
    pub static ref REGISTRY: PromReg = PromReg::new();
    pub static ref TOTAL_ACTIVE: IntGauge =
        IntGauge::new("total_active", "Total active sessions").expect("metric can be created");
}

#[cfg(test)]
mod tests {
    use crate::Registry;

    #[tokio::test]
    async fn it_works() {
        let reg = Registry::new();
        let mut h1 = reg.register("foo").await.unwrap();
        assert_eq!(1, h1.next().await.unwrap());
        reg.stop().await.unwrap();
    }
}

#[derive(Debug)]
pub struct Handle {
    id: u64,

    // TODO: optimize this so that not all handles keep a
    // copy of the key.
    key: String,

    ch: mpsc::Receiver<u64>,
    control: mpsc::Sender<Request>,
}

impl Handle {
    pub async fn next(&mut self) -> Option<u64> {
        self.ch.recv().await
    }

    pub async fn close(self) {
        let ch = self.control.clone();
        ch.send(Request::Unregister(self))
            .await
            .expect("failed to send unregister");
    }
}

#[derive(Debug)]
pub enum Request {
    Register(String, mpsc::Sender<Handle>),
    Unregister(Handle),
    #[cfg(test)]
    Stop,
}

pub struct Registry {
    ch: mpsc::Sender<Request>,
    _join: tokio::task::JoinHandle<()>,
}

impl Registry {
    pub fn new() -> Registry {
        REGISTRY
            .register(Box::new(TOTAL_ACTIVE.clone()))
            .expect("can register");
        let (tx, rx) = mpsc::channel(10);
        Registry {
            ch: tx.clone(),
            _join: tokio::spawn(async move { Self::main(tx.clone(), rx).await }),
        }
    }

    async fn publish(txm: &HashMap<u64, mpsc::Sender<u64>>, set: &HashSet<u64>, v: u64) {
        for id in set {
            debug!("Sending to id {}", id);
            let e = txm.get(id);
            if e.is_none() {
                warn!("Wanted to publish to channel ID {}, but missing", id);
                continue;
            }
            if let Err(err) = e.unwrap().send(v).await {
                warn!("Failed to publish to channel ID {}: {}", id, err);
                continue;
            }
        }
        debug!("Sent to all");
    }

    async fn main(tx: mpsc::Sender<Request>, mut rx: mpsc::Receiver<Request>) {
        let mut tx_map: HashMap<u64, mpsc::Sender<u64>> = HashMap::new();
        let mut key_map: HashMap<String, HashSet<u64>> = HashMap::new();
        let mut current_id = 0;
        loop {
            match rx.recv().await {
                Some(Request::Register(key, ch)) => {
                    debug!("Registering");
                    let (ctx, crx) = mpsc::channel(10);

                    let entry = key_map.entry(key.to_owned()).or_default();
                    let count = entry.len() + 1;
                    current_id += 1;
                    let id = current_id;
                    entry.insert(id);

                    let handle = Handle {
                        id,
                        key: key.clone(),
                        ch: crx,
                        control: tx.clone(),
                    };

                    tx_map.insert(id, ctx);
                    debug!("After register: {} active connections", tx_map.len());
                    Self::publish(&tx_map, entry, u64::try_from(count).unwrap()).await;
                    if let Err(err) = ch.send(handle).await {
                        warn!("Failed to send handle back during register(): {}", err);
                    };
                }
                Some(Request::Unregister(handle)) => {
                    debug!("Unregistering {}", handle.id);
                    let key = handle.key;
                    let k2 = key.clone();
                    tx_map.remove(&handle.id);
                    let remaining = u64::try_from(
                        key_map
                            .entry(key)
                            .and_modify(|e| {
                                e.remove(&handle.id);
                            })
                            .or_default()
                            .len(),
                    )
                    .expect("TODO");
                    if remaining == 0 {
                        key_map.remove(&k2);
                    } else {
                        match key_map.get(&k2) {
                            Some(set) => {
                                Self::publish(&tx_map, set, remaining).await;
                            }
                            None => {
                                warn!("CAN'T HAPPEN: Double unregister??")
                            }
                        }
                    }
                    debug!("After unregister: {} active connections", tx_map.len());
                    TOTAL_ACTIVE.set(i64::try_from(tx_map.len()).unwrap());
                }
                #[cfg(test)]
                Some(Request::Stop) => break,
                None => {
                    warn!("control channel shutting down?");
                    break;
                }
            };
        }
    }

    pub async fn register(&self, key: &str) -> Option<Handle> {
        let (tx, mut rx) = mpsc::channel(10);
        if let Err(err) = self.send(Request::Register(key.to_string(), tx)).await {
            warn!("Failed to register: {}", err);
            return None;
        }
        rx.recv().await
    }

    async fn send(&self, req: Request) -> Result<(), SendError<Request>> {
        self.ch.send(req).await
    }

    #[cfg(test)]
    pub async fn stop(self) -> Result<(), tokio::task::JoinError> {
        self.send(Request::Stop).await.expect("TODO");
        self._join.await
    }
}
