use std::fmt;

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct StreamDeckDevice {
    device: streamdeck::Kind,
    pid: u16,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum StreamDeckDeviceTypes {
    Original,
    OriginalV2,
    Mini,
    Xl,
    Mk2,
}

impl StreamDeckDevice {
    pub fn new(device_type: StreamDeckDeviceTypes) -> StreamDeckDevice {
        match device_type {
            StreamDeckDeviceTypes::Original => StreamDeckDevice {
                device: streamdeck::Kind::Original,
                pid: streamdeck::pids::ORIGINAL,
            },
            StreamDeckDeviceTypes::OriginalV2 => StreamDeckDevice {
                device: streamdeck::Kind::OriginalV2,
                pid: streamdeck::pids::ORIGINAL_V2,
            },
            StreamDeckDeviceTypes::Mini => StreamDeckDevice {
                device: streamdeck::Kind::Mini,
                pid: streamdeck::pids::MINI,
            },
            StreamDeckDeviceTypes::Xl => StreamDeckDevice {
                device: streamdeck::Kind::Xl,
                pid: streamdeck::pids::XL,
            },
            StreamDeckDeviceTypes::Mk2 => StreamDeckDevice {
                device: streamdeck::Kind::Mk2,
                pid: streamdeck::pids::MK2,
            },
        }
    }

    pub fn keys(self) -> u8 {
        return self.device.keys();
    }

    pub fn image_size(self) -> (usize, usize) {
        return self.device.image_size();
    }

    pub fn pid(self) -> u16 {
        return self.pid;
    }
}

impl fmt::Display for StreamDeckDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.device {
            Original => "Original",
            OriginalV2 => "OriginalV2",
            Mini => "Mini",
            Xl => "Xl",
            Mk2 => "Mk2",
        };
        write!(f, "{}", s)
    }
}
