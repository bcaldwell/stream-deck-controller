use crate::profiles::{self, Actions};
use crate::ws_api;
use crate::Config;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::oneshot;
use tokio::time;
use warp::{http, Filter};

#[derive(Debug)]
pub struct ExecuteActionReq {
    pub tx: oneshot::Sender<String>,
    pub actions: Actions,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ProfileButtonPressed {
    profile: String,
    button: usize,
}

pub async fn start_rest_api(
    config_ref: Arc<Config>,
    integration_manager_tx: Sender<ExecuteActionReq>,
) {
    let event_processor = warp::any().map(move || integration_manager_tx.clone());
    let with_config = warp::any().map(move || config_ref.clone());

    // GET /v1/ws -> websocket upgrade
    let ws_endpoint = warp::path("ws")
        // The `ws()` filter will prepare Websocket handshake...
        .and(warp::ws())
        .and(event_processor.clone())
        .map(|ws: warp::ws::Ws, event_processor| {
            // This will call our function if the handshake succeeds.
            ws.on_upgrade(move |socket| ws_api::ws_user_connected(socket, event_processor))
        });

    // POST /v1/actions/execute
    let execute_action_endpoint = warp::post()
        .and(warp::path("execute"))
        .and(warp::path::end())
        .and(warp::body::json())
        .and(event_processor.clone())
        .and_then(handle_execute_action);

    // POST /v1/profiles/button_press
    let execute_button_press_endpoint = warp::post()
        .and(warp::path("button_press"))
        .and(warp::path::end())
        .and(warp::body::json())
        .and(event_processor.clone())
        .and(with_config)
        .and_then(handle_button_pressed_action);

    let actions_endpoint = warp::path("actions").and(execute_action_endpoint);
    let profiles_endpoint = warp::path("profiles").and(execute_button_press_endpoint);

    let v1_endpoint = warp::path("v1").and(ws_endpoint.or(actions_endpoint).or(profiles_endpoint));

    // GET / -> index html
    let index_endpoint = warp::path::end().map(|| warp::reply::reply());

    let routes = index_endpoint.or(v1_endpoint);

    return warp::serve(routes).run(([127, 0, 0, 1], 8000)).await;
}

async fn handle_execute_action(
    actions: Actions,
    event_processor: Sender<ExecuteActionReq>,
) -> Result<impl warp::Reply, warp::Rejection> {
    match execute_action_request(actions.to_owned(), event_processor).await {
        Ok(r) => Ok(warp::reply::with_status(r, http::StatusCode::OK)),
        Err(e) => Ok(warp::reply::with_status(
            e.to_string(),
            http::StatusCode::BAD_REQUEST,
        )),
    }
}

async fn handle_button_pressed_action(
    profile_button_pressed: ProfileButtonPressed,
    event_processor: mpsc::Sender<ExecuteActionReq>,
    config: Arc<Config>,
) -> Result<impl warp::Reply, warp::Rejection> {
    println!("{:?}", profile_button_pressed);

    let actions = match get_actions_for_button_press(&config.profiles, profile_button_pressed) {
        Ok(actions) => actions,
        Err(e) => {
            return Ok(warp::reply::with_status(
                e.to_string(),
                http::StatusCode::BAD_REQUEST,
            ))
        }
    };

    match execute_action_request(actions.to_owned(), event_processor).await {
        Ok(r) => Ok(warp::reply::with_status(r, http::StatusCode::OK)),
        Err(e) => Ok(warp::reply::with_status(
            e.to_string(),
            http::StatusCode::BAD_REQUEST,
        )),
    }
}

async fn execute_action_request(
    actions: Actions,
    event_processor: mpsc::Sender<ExecuteActionReq>,
) -> Result<String> {
    println!("{:?}", actions);
    let (resp_tx, resp_rx) = oneshot::channel::<String>();

    let execute_action_req = ExecuteActionReq {
        actions: actions,
        tx: resp_tx,
    };

    event_processor.send(execute_action_req).await.unwrap();

    // Wrap the future with a `Timeout` set to expire in 5 seconds.
    match time::timeout(time::Duration::from_secs(5), resp_rx).await {
        Ok(resp) => match resp {
            Ok(msg) => return Ok(msg),
            Err(e) => Err(anyhow::Error::new(e).context("error executing actions for request.")),
        },
        Err(e) => Err(anyhow::Error::new(e).context(
            "timmed out waiting for request to complete, actions may still complete successfully.",
        )),
    }
}

fn get_actions_for_button_press(
    profiles: &profiles::Profiles,
    profile_button_pressed: ProfileButtonPressed,
) -> Result<&Actions> {
    let profile =
        match profiles::get_profile_by_name(profiles, profile_button_pressed.profile.clone()) {
            Some(profile) => profile,
            None => {
                return Err(anyhow!(
                    "profile {} not found",
                    profile_button_pressed.profile.to_string()
                ))
            }
        };

    let button = match profile.buttons.get(profile_button_pressed.button) {
        Some(button) => button,
        None => {
            return Err(anyhow!(
                "button {} not found",
                profile_button_pressed.button
            ))
        }
    };

    return Ok(&button.actions);
}
