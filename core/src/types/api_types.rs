use crate::types;
use tokio::sync::oneshot;

#[derive(Debug)]
pub struct ExecuteActionReq {
    pub tx: oneshot::Sender<String>,
    pub actions: Actions,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ProfileButtonPressed {
    pub profile: String,
    pub button: usize,
}

pub type Profiles = Vec<Profile>;
pub type Actions = Vec<Action>;

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct Action {
    pub action: String,
    #[serde(flatten)]
    pub options: serde_json::value::Value,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub name: String,
    pub buttons: Vec<ProfileButton>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ProfileButton {
    pub states: Option<Vec<types::SetButtonUI>>,
    pub actions: Actions,
}
