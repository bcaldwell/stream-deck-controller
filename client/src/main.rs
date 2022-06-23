use core::types::ProfileButtonPressed;
use rand::Rng;
use std::{env, str::FromStr};
use streamdeck::{Colour, Error, Filter, ImageOptions, StreamDeck};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
// use tokio::sync::mpsc::{self, Receiver, Sender};
use futures_util::stream::StreamExt;
use futures_util::SinkExt;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[tokio::main]
async fn main() {
    // Connect to device
    // let filter = Filter {};
    let vid = 0x0fd9;
    let mut deck = match StreamDeck::connect(vid, streamdeck::pids::MK2, None) {
        Ok(d) => d,
        Err(e) => {
            println!("Error connecting to streamdeck: {:?}", e);
            return;
        }
    };

    let serial = deck.serial().unwrap();
    println!(
        "Connected to device (vid: {:04x} pid: {:04x} serial: {})",
        vid,
        streamdeck::pids::MK2,
        serial
    );

    let root_url = env::var("STREAM_DECK_API_URL").unwrap_or("127.0.0.1:8000".to_string());

    let (ws_stream, _) = connect_async(format!("ws://{}/v1/ws", root_url))
        .await
        .expect("Failed to connect");

    deck.set_blocking(true);
    // let mut rng = rand::thread_rng();
    // for i in 0..15 {
    //     let color = format!(
    //         "{:02X}{:02X}{:02X}",
    //         rng.gen_range(1..256),
    //         rng.gen_range(1..256),
    //         rng.gen_range(1..256)
    //     );
    //     println!("{} {}", i, color);
    //     deck.set_button_rgb(i, &Colour::from_str(&color).unwrap());
    // }

    // let (tx, rx) = mpsc::unbounded_channel();

    // tokio::spawn(listen_for_button_press(tx))
    println!("WebSocket handshake has been successfully completed");

    let (mut write, read) = ws_stream.split();

    // let stdin_to_ws = stdin_rx.map(Ok).forward(write);
    // let ws_to_stdout = {
    //     read.for_each(|message| async {
    //         let data = message.unwrap().into_data();
    //         tokio::io::stdout().write_all(&data).await.unwrap();
    //     })
    // };

    loop {
        let button_state = deck.read_buttons(None).unwrap();

        for (i, state) in button_state.iter().enumerate() {
            if state.eq(&0) {
                continue;
            }

            println!("{}", i);
            let map = ProfileButtonPressed {
                profile: "default".to_string(),
                button: i,
            };

            write
                .send(Message::text(serde_json::to_string(&map).unwrap()))
                .await
                .unwrap();

            // let client = reqwest::blocking::Client::new();
            // let root_url =
            //     env::var("STREAM_DECK_API_URL").unwrap_or("http://127.0.0.1:8000".to_string());
            // let res = client
            //     .post(format!("{}/v1/profiles/button_press", root_url))
            //     .json(&map)
            //     .header("Content-Type", "application/json")
            //     .send();
            // println!("{:?}", res);
        }

        println!("{:?}", deck.read_buttons(None));
    }
}
