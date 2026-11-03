Vendored copy of elgato-streamdeck 0.14.0 (MIT) with a change in Cargo.toml:
the hidapi dependency uses `default-features = false, features = ["linux-native-basic-udev"]`
instead of the default `linux-static-hidraw` (which needs libudev + a C toolchain and
breaks static musl cross builds). Wired in via /.cargo/config.toml [patch.crates-io].

And one addition: `StreamDeck::connect_path` opens a hidraw node by path with a process-wide
`HidApi`. `HidApi::new()` enumerates all HID devices (and `open_serial` does too), which takes
over a second on a Raspberry Pi Zero 2 W because hidapi's native backend reads every sysfs
attribute of the deck's ancestors, including slow ones of the USB controller. Opening by path
on the shared instance costs a millisecond, which matters when reopening after a USB suspend.
