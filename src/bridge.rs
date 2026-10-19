use crate::engine::MidiRouter;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::Value;
use std::{
    net::TcpListener as StdTcpListener,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, oneshot},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        handshake::server::{Request, Response},
        Message,
    },
    WebSocketStream,
};

const MAX_CLIENTS: usize = 16;
const EVENT_BUFFER_SIZE: usize = 8192;

pub struct Bridge {
    pub events: broadcast::Sender<Value>,
    pub clients: Arc<AtomicUsize>,
    pub router: Arc<Mutex<MidiRouter>>,
    pub adapter_url: String,
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Bridge {
    pub fn start() -> Result<Self, String> {
        let mut token_bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut token_bytes);
        let token: String = token_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let listener = StdTcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let url = format!("ws://127.0.0.1:{port}/{token}");
        let script = include_str!("../assets/browser.js").replace("__MIDI_BRIDGE_URL__", &url);
        let adapter_url = format!("{url}/adapter");

        let router = Arc::new(Mutex::new(MidiRouter::default()));
        let (events, _) = broadcast::channel(EVENT_BUFFER_SIZE);
        let clients = Arc::new(AtomicUsize::new(0));
        let (shutdown, stop) = oneshot::channel();
        let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
        let server = serve(
            listener,
            token,
            Arc::new(script),
            events.clone(),
            clients.clone(),
            router.clone(),
            stop,
        );
        let worker = std::thread::spawn(move || runtime.block_on(server));

        Ok(Self {
            events,
            clients,
            router,
            adapter_url,
            shutdown: Some(shutdown),
            worker: Some(worker),
        })
    }

    pub fn send(&self, events: Vec<Value>) {
        for event in events {
            let _ = self.events.send(event);
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(stop) = self.shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn serve(
    listener: StdTcpListener,
    token: String,
    script: Arc<String>,
    events: broadcast::Sender<Value>,
    clients: Arc<AtomicUsize>,
    router: Arc<Mutex<MidiRouter>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let listener = TcpListener::from_std(listener).expect("validated listener");
    loop {
        let connection = tokio::select! {
            _ = &mut shutdown => break,
            connection = listener.accept() => connection,
        };
        let Ok((stream, _)) = connection else {
            break;
        };
        if clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
            continue;
        }

        tokio::spawn(handle_client(
            stream,
            format!("/{token}"),
            format!("/{token}/adapter"),
            script.clone(),
            events.clone(),
            clients.clone(),
            router.clone(),
        ));
    }

    let _ = events.send(serde_json::json!({ "type": "panic" }));
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn handle_client(
    stream: TcpStream,
    note_path: String,
    adapter_path: String,
    script: Arc<String>,
    events: broadcast::Sender<Value>,
    clients: Arc<AtomicUsize>,
    router: Arc<Mutex<MidiRouter>>,
) {
    let mut is_adapter = false;
    // Tungstenite requires its error response by value.
    #[allow(clippy::result_large_err)]
    let handshake = accept_hdr_async(stream, |request: &Request, response: Response| {
        is_adapter = request.uri().path() == adapter_path;
        if request.uri().path() != note_path && !is_adapter {
            return Err(Response::builder()
                .status(403)
                .body(Some("Wrong session token".into()))
                .unwrap());
        }
        Ok(response)
    });
    let mut socket = match tokio::time::timeout(Duration::from_secs(3), handshake).await {
        Ok(Ok(socket)) => socket,
        _ => return,
    };

    if is_adapter {
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            socket
                .send(Message::Text(script.as_ref().clone().into()))
                .await?;
            socket.close(None).await
        })
        .await;
        return;
    }

    let (receiver, channel_info) = {
        let router = router.lock().unwrap();
        (events.subscribe(), router.channel_info())
    };
    let send = socket.send(Message::Text(channel_info.to_string().into()));
    if !matches!(
        tokio::time::timeout(Duration::from_millis(500), send).await,
        Ok(Ok(()))
    ) {
        return;
    }
    clients.fetch_add(1, Ordering::Relaxed);
    forward_events(socket, receiver).await;
    clients.fetch_sub(1, Ordering::Relaxed);
}

async fn forward_events(
    socket: WebSocketStream<TcpStream>,
    mut events: broadcast::Receiver<Value>,
) {
    let (mut output, mut input) = socket.split();
    loop {
        tokio::select! {
            message = input.next() => {
                match message {
                    Some(Ok(Message::Ping(data))) => {
                        if output.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                let (event, close_after_send) = match event {
                    Ok(event) => (event, false),
                    Err(_) => (serde_json::json!({ "type": "panic" }), true),
                };
                let send = output.send(Message::Text(event.to_string().into()));
                let result = tokio::time::timeout(Duration::from_millis(500), send).await;
                if !matches!(result, Ok(Ok(()))) || close_after_send {
                    break;
                }
            }
        }
    }
}
