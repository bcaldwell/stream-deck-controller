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
    // pub states: Vec<ButtonState>,
    pub actions: Actions,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ButtonState {}

pub fn get_profile_by_name(profiles: &Profiles, name: String) -> Option<&Profile> {
    for profile in profiles {
        if profile.name == name {
            return Some(profile);
        }
    }

    return None;
}
