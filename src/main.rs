use std::collections::{HashMap, HashSet};
use std::sync::{atomic, Arc};

use log::LevelFilter;
use simplelog::{Config, TermLogger};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{self, RwLock};
use tokio::{io, time};

use maelstrom::{ids, protocol};

#[tokio::main]
async fn main() {
    TermLogger::init(
        LevelFilter::Info,
        Config::default(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )
    .expect("Logger init error");

    log::info!("Node starting");

    let mut requests_rx = read_incoming_messages().await;
    let (responses_tx, mut responses_rx) = sync::mpsc::channel(100);

    let handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(response) = responses_rx.recv().await {
            let raw = serde_json::to_string(&response).expect("JSON serialize error");
            stdout.write_all(raw.as_bytes()).await.expect("IO error");
            stdout.write_all(b"\n").await.expect("IO error");
        }
    });

    if let Some(node) = wait_for_init(&mut requests_rx, responses_tx).await {
        log::info!("Node initialized as {}", node.id);
        listen(node, &mut requests_rx).await;
    }

    handle.await.expect("Task error");

    log::info!("Node shutting down");
}

async fn read_incoming_messages() -> sync::mpsc::Receiver<protocol::Message> {
    let (tx, rx) = sync::mpsc::channel(100);
    tokio::spawn(async move {
        let stdin = io::stdin();
        let mut lines = io::BufReader::new(stdin).lines();
        while let Some(line) = lines.next_line().await.expect("IO error") {
            let message =
                serde_json::from_str::<protocol::Message>(&line).expect("JSON parse error");
            tx.send(message).await.expect("Channel error");
        }
    });
    rx
}

async fn wait_for_init(
    messages_rx: &mut sync::mpsc::Receiver<protocol::Message>,
    responses_tx: sync::mpsc::Sender<protocol::Message>,
) -> Option<Node> {
    while let Some(message) = messages_rx.recv().await {
        if let protocol::Payload::Init { node_id, .. } = message.body.value {
            let msg = protocol::Message {
                src: node_id.into(),
                dest: message.src,
                body: protocol::Body {
                    msg_id: None,
                    in_reply_to: message.body.msg_id,
                    value: protocol::Payload::InitOk {},
                },
            };
            responses_tx.send(msg).await.expect("Channel error");
            return Some(Node::new(node_id, responses_tx.clone()));
        }
    }
    None
}

async fn listen(node: Node, messages_rx: &mut sync::mpsc::Receiver<protocol::Message>) {
    while let Some(message) = messages_rx.recv().await {
        let node = node.clone();
        tokio::spawn(async move {
            node.handle_message(&message).await;
        });
    }
}

#[derive(Clone)]
struct Node {
    id: ids::NodeId,

    ids_counter: Arc<atomic::AtomicU64>,
    messages: Arc<RwLock<HashSet<ids::MessageId>>>,
    topology: Arc<RwLock<HashMap<ids::NodeId, HashSet<ids::NodeId>>>>,

    waiting_for: Arc<RwLock<HashMap<ids::MessageId, sync::oneshot::Sender<protocol::Message>>>>,

    out: sync::mpsc::Sender<protocol::Message>,
}

impl Node {
    fn new(id: ids::NodeId, out: sync::mpsc::Sender<protocol::Message>) -> Self {
        Self {
            id,
            ids_counter: Arc::new(atomic::AtomicU64::new(0)),
            messages: Arc::new(RwLock::new(HashSet::new())),
            topology: Arc::new(RwLock::new(HashMap::new())),
            waiting_for: Arc::new(RwLock::new(HashMap::new())),
            out,
        }
    }

    fn generate_id(&self) -> ids::MessageId {
        let counter = self.ids_counter.fetch_add(1, atomic::Ordering::SeqCst);
        let id = u64::from(self.id) << 32 | counter;
        id.into()
    }

    #[allow(dead_code)]
    async fn broadcast(&self, dest: ids::NodeId, value: protocol::Payload) {
        self.out
            .send(protocol::Message {
                src: self.id.into(),
                dest: dest.into(),
                body: protocol::Body {
                    msg_id: None,
                    in_reply_to: None,
                    value,
                },
            })
            .await
            .expect("Channel error");
    }

    async fn send(
        &self,
        dest: ids::NodeId,
        value: protocol::Payload,
        is_successful: impl Fn(&protocol::Payload) -> bool,
    ) {
        let msg_id = self.generate_id();

        let mut timeout_ms = 16;
        loop {
            let (tx, rx) = sync::oneshot::channel::<protocol::Message>();
            self.waiting_for.write().await.insert(msg_id, tx);

            self.out
                .send(protocol::Message {
                    src: self.id.into(),
                    dest: dest.into(),
                    body: protocol::Body {
                        msg_id: Some(msg_id),
                        in_reply_to: None,
                        value: value.clone(),
                    },
                })
                .await
                .expect("Channel error");

            let reply = time::timeout(time::Duration::from_millis(timeout_ms), rx).await;

            match reply {
                Ok(msg) => {
                    let msg = msg.expect("Channel error");
                    if is_successful(&msg.body.value) {
                        break;
                    }
                    log::warn!("Got an error reply to message {}, will retry", msg_id);
                }
                Err(_) => {
                    log::warn!(
                        "Timeout waiting for reply to message {}, will retry",
                        msg_id
                    );
                }
            }

            self.waiting_for.write().await.remove(&msg_id);

            timeout_ms *= 2;
        }
    }

    async fn reply(&self, src: &protocol::Message, value: protocol::Payload) {
        if let Some(in_reply_to) = src.body.msg_id {
            let msg = protocol::Message {
                src: self.id.into(),
                dest: src.src,
                body: protocol::Body {
                    msg_id: None,
                    in_reply_to: Some(in_reply_to),
                    value,
                },
            };
            self.out.send(msg).await.expect("Channel error");
        }
    }

    async fn handle_message(&self, msg: &protocol::Message) {
        if msg.dest != self.id.into() {
            // Ignore messages not addressed to this node
            return;
        }

        if let Some(in_reply_to) = msg.body.in_reply_to {
            // Check if this node is waiting for a reply to this message
            if let Some(tx) = self.waiting_for.write().await.remove(&in_reply_to) {
                tx.send(msg.clone()).expect("Channel error");
            } else {
                self.reply(
                    msg,
                    protocol::Payload::Error {
                        code: protocol::ErrorCode::PreconditionFailed,
                        text: "Got a reply to a message that was not sent".to_string(),
                    },
                )
                .await;
            }
        } else {
            match &msg.body.value {
                protocol::Payload::Echo { echo } => {
                    self.reply(msg, protocol::Payload::EchoOk { echo: echo.clone() })
                        .await;
                }
                protocol::Payload::Generate {} => {
                    let id = self.generate_id();
                    self.reply(msg, protocol::Payload::GenerateOk { id }).await;
                }
                protocol::Payload::Broadcast { message } => {
                    if self.messages.read().await.contains(message) {
                        // if the message is already seen, do not broadcast it
                        self.reply(msg, protocol::Payload::BroadcastOk {}).await;
                    } else {
                        {
                            self.messages.write().await.insert(*message);
                        }
                        self.reply(msg, protocol::Payload::BroadcastOk {}).await;

                        let broadcast_to = if let ids::PeerId::Node(src_node_id) = msg.src {
                            // if the message came from a node, broadcast to all neighbors except the sender
                            let topology = self.topology.read().await;
                            let neighbors = topology.get(&self.id).cloned().unwrap_or_default();
                            neighbors.into_iter().filter(|node_id| *node_id != src_node_id).collect::<Vec<_>>()
                        } else {
                            // if the message came from a client, broadcast to all neighbors
                            self.topology
                                .read()
                                .await
                                .get(&self.id)
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .collect::<Vec<_>>()
                        };

                        let broadcasts = broadcast_to.into_iter().map(|node_id| {
                            self.send(
                                node_id,
                                protocol::Payload::Broadcast { message: *message },
                                |payload| matches!(payload, protocol::Payload::BroadcastOk {}),
                            )
                        });

                        futures::future::join_all(broadcasts).await;
                    }
                }
                protocol::Payload::Read {} => {
                    let messages = self.messages.read().await.iter().copied().collect();
                    self.reply(msg, protocol::Payload::ReadOk { messages })
                        .await;
                }
                protocol::Payload::Topology { ref topology } => {
                    *self.topology.write().await = topology.clone();
                    self.reply(msg, protocol::Payload::TopologyOk {}).await;
                }
                _ => {
                    self.reply(
                        msg,
                        protocol::Payload::Error {
                            code: protocol::ErrorCode::NotSupported,
                            text: "Not supported".to_string(),
                        },
                    )
                    .await
                }
            }
        }
    }
}
