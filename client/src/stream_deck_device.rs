use std::fmt;

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct StreamDeckDevice {
    internal_type: StreamDeckDeviceTypes,
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
                internal_type: device_type,
                device: streamdeck::Kind::Original,
                pid: streamdeck::pids::ORIGINAL,
            },
            StreamDeckDeviceTypes::OriginalV2 => StreamDeckDevice {
                internal_type: device_type,
                device: streamdeck::Kind::OriginalV2,
                pid: streamdeck::pids::ORIGINAL_V2,
            },
            StreamDeckDeviceTypes::Mini => StreamDeckDevice {
                internal_type: device_type,
                device: streamdeck::Kind::Mini,
                pid: streamdeck::pids::MINI,
            },
            StreamDeckDeviceTypes::Xl => StreamDeckDevice {
                internal_type: device_type,
                device: streamdeck::Kind::Xl,
                pid: streamdeck::pids::XL,
            },
            StreamDeckDeviceTypes::Mk2 => StreamDeckDevice {
                internal_type: device_type,
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
        let s = match self.internal_type {
            StreamDeckDeviceTypes::Original => "Original",
            StreamDeckDeviceTypes::OriginalV2 => "OriginalV2",
            StreamDeckDeviceTypes::Mini => "Mini",
            StreamDeckDeviceTypes::Xl => "Xl",
            StreamDeckDeviceTypes::Mk2 => "Mk2",
        };
        write!(f, "{}", s)
    }
}
