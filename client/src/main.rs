use anyhow::{anyhow, Result};
use core::types::{ProfileButtonPressed, SetButtonUI, WsActions};
use futures_util::stream::{self, SplitSink, StreamExt};
use futures_util::SinkExt;
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use streamdeck::{Colour, StreamDeck};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
mod stream_deck_device;
use stream_deck_device::{StreamDeckDevice, StreamDeckDeviceTypes};

struct SetButtonRequest {
    state: SetButtonUI,
    button: u8,
}

#[tokio::main]
async fn main() {
    let device = StreamDeckDevice::new(StreamDeckDeviceTypes::Mk2);

    let deck_ref = connect_to_stream_deck(device)
        .await
        .expect("connecting to streamdeck failed");

    let root_url = env::var("STREAM_DECK_API_URL").unwrap_or("127.0.0.1:8000".to_string());
    println!(
        "using {} as the stream deck url, can be set via `STREAM_DECK_API_URL` env var",
        &root_url
    );

    let (ws_stream, _) = connect_async(format!("ws://{}/v1/ws", root_url))
        .await
        .expect("Failed to connect to server");
    println!("WebSocket handshake has been successfully completed");

    let (image_update_tx, image_update_rx) = mpsc::unbounded_channel::<SetButtonRequest>();
    let (write, read) = ws_stream.split();

    let handle_button_requests_join = tokio::spawn(handle_set_button_requests(
        image_update_rx,
        deck_ref.clone(),
        device,
    ));

    let stream_deck_listener_join = tokio::spawn(start_stream_deck_listener(deck_ref, write));

    read.for_each(|msg| async {
        handle_socket_message(msg.unwrap(), image_update_tx.clone(), &device).await;
    })
    .await;

    handle_button_requests_join.await.unwrap();
    stream_deck_listener_join.await.unwrap();
}

async fn connect_to_stream_deck(device: StreamDeckDevice) -> Result<Arc<Mutex<StreamDeck>>> {
    // Connect to device
    // this is the vid for stream deck
    let vid = 0x0fd9;
    let deck = StreamDeck::connect(vid, device.pid(), None)
        .map_err(|e| anyhow!("error connecting to streamdeck: {:?}", e))?;
    let deck_ref = Arc::new(Mutex::new(deck));

    let serial = deck_ref
        .lock()
        .await
        .serial()
        .map_err(|e| anyhow!("failed to get serial for stream deck connection: {:?}", e))?;
    println!(
        "Connected to device (vid: {:04x} pid: {:04x} serial: {} name: {})",
        vid,
        &device.pid(),
        serial,
        &device,
    );

    deck_ref
        .lock()
        .await
        .set_blocking(false)
        .map_err(|e| anyhow!("failed to set streamdeck into blocking mode: {}", e))?;

    return Ok(deck_ref);
}

async fn handle_set_button_requests(
    mut rx: mpsc::UnboundedReceiver<SetButtonRequest>,
    deck_ref: Arc<Mutex<StreamDeck>>,
    device: StreamDeckDevice,
) {
    while let Some(set_button_request) = rx.recv().await {
        match set_button_state(&set_button_request, &deck_ref, &device).await {
            Ok(_) => (),
            Err(e) => {
                println!(
                    "failed to set button {} image: {}",
                    set_button_request.button, e
                )
            }
        }
    }
}

async fn set_button_state(
    set_button_request: &SetButtonRequest,
    deck_ref: &Arc<Mutex<StreamDeck>>,
    device: &StreamDeckDevice,
) -> Result<()> {
    if let Some(image) = &set_button_request.state.image {
        let img_str = base64::decode(image.to_string().into_bytes())
            .map_err(|e| anyhow!("failed to decode image from base64: {}", e))?;
        let image = image::load_from_memory(&img_str)
            .map_err(|e| anyhow!("failed to load image from into memory: {}", e))?;

        let (w, h) = device.image_size();
        let resized_image = image.resize(
            w.try_into().unwrap(),
            h.try_into().unwrap(),
            image::imageops::FilterType::Nearest,
        );

        deck_ref
            .lock()
            .await
            .set_button_image(set_button_request.button, resized_image)
            .map_err(|e| anyhow!("failed to set button image: {}", e))?;

        return Ok(());
    }

    if let Some(color_str) = &set_button_request.state.color {
        let color = Colour::from_str(color_str)
            .map_err(|e| anyhow!("invalid color {}: {}", color_str, e))?;

        deck_ref
            .lock()
            .await
            .set_button_rgb(set_button_request.button, &color)
            .map_err(|e| anyhow!("failed to set button color: {}", e))?;

        return Ok(());
    }

    Ok(())
}

async fn handle_socket_message(
    msg: Message,
    image_update_tx: mpsc::UnboundedSender<SetButtonRequest>,
    device: &StreamDeckDevice,
) {
    let p: WsActions = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    let r = match p {
        WsActions::SetButton { index, button } => image_update_tx
            .send(SetButtonRequest {
                state: button,
                button: index,
            })
            .map_err(|e| anyhow!("{}", e)),
        WsActions::SetButtons { buttons } => {
            send_button_update_requests(image_update_tx, buttons, device).await
        }
        _ => Err(anyhow!("unknown message")),
    };
    match r {
        Ok(_) => (),
        Err(e) => println!("{}", e),
    };
}

async fn send_button_update_requests(
    image_update_tx: mpsc::UnboundedSender<SetButtonRequest>,
    buttons: Vec<SetButtonUI>,
    device: &StreamDeckDevice,
) -> Result<()> {
    for (i, button) in buttons.iter().enumerate() {
        image_update_tx
            .send(SetButtonRequest {
                state: button.clone(),
                button: i as u8,
            })
            .map_err(|e| anyhow!("{}", e))?
    }

    // set remaining buttons black
    for i in buttons.len()..device.keys().try_into().unwrap() {
        image_update_tx
            .send(SetButtonRequest {
                state: SetButtonUI {
                    color: Some("000000".to_string()),
                    image: None,
                },
                button: i as u8,
            })
            .map_err(|e| anyhow!("{}", e))?
    }

    Ok(())
}

async fn start_stream_deck_listener(
    deck_ref: Arc<Mutex<StreamDeck>>,
    mut write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
) {
    loop {
        let button_state_option = read_stream_deck(&deck_ref).await;

        if let Some(button_state) = button_state_option {
            for (i, state) in button_state.iter().enumerate() {
                if state.eq(&0) {
                    continue;
                }

                let map = ProfileButtonPressed {
                    profile: None,
                    button: i,
                };

                write
                    .send(Message::text(serde_json::to_string(&map).unwrap()))
                    .await
                    .unwrap();
            }
        }

        sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn read_stream_deck(deck_ref: &Arc<Mutex<StreamDeck>>) -> Option<Vec<u8>> {
    let states = deck_ref.lock().await.read_buttons(None);
    match states {
        Ok(states) => Some(states),
        Err(e) => match e {
            streamdeck::Error::NoData => None,
            _ => {
                println!("failed to read from streamdeck: {}", e);
                None
            }
        },
    }
}
