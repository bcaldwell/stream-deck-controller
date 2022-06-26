use sdc_core::types::{Profile, Profiles};

pub fn get_profile_by_name(profiles: &Profiles, name: String) -> Option<&Profile> {
    for profile in profiles {
        if profile.name == name {
            return Some(profile);
        }
    }

    return None;
}
