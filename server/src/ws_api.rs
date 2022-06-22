use crate::Config;
use core::types::{ExecuteActionReq, ProfileButtonPressed};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::mpsc::{self};
use warp::ws::WebSocket;

pub async fn ws_user_connected(
    ws: WebSocket,
    event_processor: mpsc::Sender<ExecuteActionReq>,
    config: Arc<Config>,
) {
    // Use a counter to assign a new unique ID for this user.
    // let my_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);

    // eprintln!("new chat user: {}", my_id);

    // Split the socket into a sender and receive of messages.
    let (mut _tx, mut rx) = ws.split();

    // Use an unbounded channel to handle buffering and flushing of messages
    // to the websocket...
    // let mut rx = UnboundedReceiverStream::new(rx);

    // tokio::task::spawn(async move {
    //     while let Some(message) = rx.next().await {
    //         user_ws_tx
    //             .send(message)
    //             .unwrap_or_else(|e| {
    //                 eprintln!("websocket send error: {}", e);
    //             })
    //             .await;
    //     }
    // });

    // Save the sender in our list of connected users.
    // users.write().await.insert(my_id, tx);

    // Return a `Future` that is basically a state machine managing
    // this specific user's connection.

    // Every time the user sends a message, broadcast it to
    // all other users...
    while let Some(result) = rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", "my_id", e);
                break;
            }
        };

        if !msg.is_text() {
            continue;
        }

        let msg_str = String::from_utf8(msg.into_bytes().to_vec()).unwrap();
        let p: ProfileButtonPressed = serde_json::from_str(&msg_str).unwrap();

        println!("{:?}", &p);
        crate::rest_api::handle_button_pressed_action(p, event_processor.clone(), config.clone())
            .await;

        // event_processor.send(Vec::new()).await.unwrap();
    }

    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
    user_disconnected(0).await;
}

// async fn handle_msg(result: Result<Message, Error>) -> Result<()> {

// }

async fn user_disconnected(my_id: usize) {
    eprintln!("good bye user: {}", my_id);

    // Stream closed up, so remove from the user list
    // users.write().await.remove(&my_id);
}
