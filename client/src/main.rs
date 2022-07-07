use anyhow::{anyhow, Result};
use futures_util::stream::StreamExt;
use futures_util::FutureExt;
use sdc_core::types::{ProfileButtonPressed, SetButtonUI, WsActions};
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use stream_deck_device::{StreamDeckDevice, StreamDeckDeviceTypes};
use streamdeck::{Colour, StreamDeck};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info};
use tracing_subscriber;

mod stream_deck_device;

const STREAM_DECK_API_URL_VAR: &str = "STREAM_DECK_API_URL";
const STREAM_DECK_BRIGHTNESS_VAR: &str = "STREAM_DECK_BRIGHTNESS";
const STREAM_DECK_SLEEP_TIMEOUT_MIN_VAR: &str = "STREAM_DECK_SLEEP_TIMEOUT_MIN";

const STREAMDECK_DEFAULT_BRIGHTNESS: u8 = 50;
const SCREEN_SLEEP_MIN: u64 = 5;
const MIN_TO_SEC: u64 = 60;

struct SetButtonRequest {
    state: SetButtonUI,
    button: u8,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let device = StreamDeckDevice::new(StreamDeckDeviceTypes::Mk2);

    let deck_ref = connect_to_stream_deck(device)
        .await
        .expect("connecting to streamdeck failed");

    let root_url = env::var(STREAM_DECK_API_URL_VAR).unwrap_or("ws://127.0.0.1:8000".to_string());
    info!(
        root_url,
        "connecting to stream deck api, url can be set via `{}` env var", STREAM_DECK_API_URL_VAR
    );

    let (ws_stream, _) = connect_async(format!("{}/v1/ws", root_url))
        .await
        .expect("Failed to connect to server");
    info!("WebSocket handshake has been successfully completed");

    // set brightness correctly on initial boot
    set_stream_deck_brightness(&deck_ref, desired_stream_deck_brightness())
        .await
        .expect("failed to set streamdeck brightness, check streamdeck connection");

    let (image_update_tx, image_update_rx) = mpsc::unbounded_channel::<SetButtonRequest>();
    let (ws_write, ws_read) = ws_stream.split();
    let (client_sender, client_rcv) = mpsc::unbounded_channel();

    let client_rcv = UnboundedReceiverStream::new(client_rcv);
    tokio::task::spawn(client_rcv.map(|x| Ok(x)).forward(ws_write).map(|result| {
        if let Err(e) = result {
            error!("error sending websocket msg: {}", e);
        }
    }));

    let handle_button_requests_join = tokio::spawn(handle_set_button_requests(
        image_update_rx,
        deck_ref.clone(),
        device,
    ));

    let stream_deck_listener_join =
        tokio::spawn(start_stream_deck_listener(deck_ref, client_sender.clone()));
    ws_read
        .for_each(|message| async {
            // tokio_tungstenite responds to ping with pong already, no need to worry about it
            let message = message.unwrap();
            handle_socket_message(message, image_update_tx.clone(), &device).await;
            // match message {
            // Ok(msg) => handle_socket_message(msg, image_update_tx.clone(), &device).await,
            //     Err(err) => {
            //         info!("Error opening message: {}", err);
            //         return Err(anyhow!("connection likely closed"));
            //     }
            // }
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
    info!(
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
                info!(
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
    if msg.is_ping() || msg.is_pong() {
        return;
    }

    if !msg.is_text() {
        info!("unknown message type, ignoring");
        return;
    }

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
        Err(e) => info!("{}", e),
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
    write: mpsc::UnboundedSender<Message>,
) {
    let mut last_button_press_time = std::time::SystemTime::now();
    let mut is_asleep = false;
    let sleep_timeout = env::var(STREAM_DECK_SLEEP_TIMEOUT_MIN_VAR)
        .unwrap_or("".to_string())
        .parse::<u64>()
        .unwrap_or(SCREEN_SLEEP_MIN);
    let sleep_timeout = std::time::Duration::from_secs(sleep_timeout * MIN_TO_SEC);
    let stream_deck_brightness = desired_stream_deck_brightness();

    loop {
        let button_state_option = read_stream_deck(&deck_ref).await;

        if let Some(button_state) = button_state_option {
            for (i, state) in button_state.iter().enumerate() {
                if state.eq(&0) {
                    continue;
                }

                last_button_press_time = std::time::SystemTime::now();
                if is_asleep {
                    is_asleep = false;
                    info!("waking up from sleep");
                    set_stream_deck_brightness(&deck_ref, stream_deck_brightness)
                        .await
                        .unwrap_or_else(|e| info!("{}", e));
                    break;
                }

                let map = ProfileButtonPressed {
                    profile: None,
                    button: i,
                };

                let msg = Message::text(serde_json::to_string(&map).unwrap());
                info!("Sending button press: {:?}", msg);
                write.send(msg).unwrap();
            }
        }

        if is_time_to_toggle_sleep(is_asleep, last_button_press_time, sleep_timeout) {
            is_asleep = true;
            info!("going to sleep");
            set_stream_deck_brightness(&deck_ref, 0)
                .await
                .unwrap_or_else(|e| info!("{}", e));
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
                info!("failed to read from streamdeck: {}", e);
                None
            }
        },
    }
}

async fn set_stream_deck_brightness(
    deck_ref: &Arc<Mutex<StreamDeck>>,
    brightness: u8,
) -> Result<()> {
    let mut deck = deck_ref.lock().await;
    deck.set_brightness(brightness).map_err(|e| {
        anyhow!(
            "failed to set streamdeck brightness to {}: {}",
            brightness,
            e
        )
    })
}

// is_time_to_toggle_sleep returns true if it is time to transition from awake to asleep
fn is_time_to_toggle_sleep(
    is_asleep: bool,
    last_button_press_time: std::time::SystemTime,
    sleep_timeout: std::time::Duration,
) -> bool {
    // no change needed
    if is_asleep {
        return false;
    }

    let time_since_last_sleep =
        match std::time::SystemTime::now().duration_since(last_button_press_time) {
            Ok(t) => t,
            Err(e) => {
                print!(
                    "failed to determine the time since last sleep, assuming no change: {}",
                    e
                );
                return false;
            }
        };

    return time_since_last_sleep > sleep_timeout;
}

fn desired_stream_deck_brightness() -> u8 {
    return env::var(STREAM_DECK_BRIGHTNESS_VAR)
        .unwrap_or("".to_string())
        .parse::<u8>()
        .unwrap_or(STREAMDECK_DEFAULT_BRIGHTNESS);
}
