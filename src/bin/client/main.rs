use streamdeck::{Colour, Error, Filter, ImageOptions, StreamDeck};

fn main() {
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
}
