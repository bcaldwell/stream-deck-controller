pub struct LightState {
    pub on: bool,
    pub brightness: Option<f32>,
}

pub fn calc_light_state(
    current_brightness: Option<f32>,
    brightness_option: Option<f32>,
    rel_brightness_option: Option<f32>,
) -> LightState {
    // when brightness and rel_brightness is none, just turn off the light
    // otherwise check brightness:
    //   if it is 0, and turn off the light
    //   otherwise, turn on the light, then set the brightness
    // then check rel_brightness
    // setting brightness first results in: device (light) is "soft off", command (.dimming.brightness) may not have effect
    if rel_brightness_option.is_none() && brightness_option.is_none() {
        return LightState {
            on: false,
            brightness: None,
        };
    }

    let brightness = match brightness_option {
        Some(b) => b,
        None => determine_rel_brightness_val(current_brightness, rel_brightness_option),
    };

    if brightness == 0.0 {
        return LightState {
            on: false,
            brightness: None,
        };
    }

    return LightState {
        on: true,
        brightness: Some(brightness),
    };
}

fn determine_rel_brightness_val(
    current_brightness: Option<f32>,
    rel_brightness_option: Option<f32>,
) -> f32 {
    // default to 0, aka do nothing
    let current_brightness = current_brightness.unwrap_or(0.0);
    let rel_brightness = rel_brightness_option.unwrap_or(0.0);

    // not sure what to do here...
    let desired_brightness = current_brightness + rel_brightness;
    return desired_brightness.min(100.0).max(0.0);
}
