Vendored copy of elgato-streamdeck 0.14.0 (MIT) with one change in Cargo.toml:
the hidapi dependency uses `default-features = false, features = ["linux-native-basic-udev"]`
instead of the default `linux-static-hidraw` (which needs libudev + a C toolchain and
breaks static musl cross builds). Wired in via /.cargo/config.toml [patch.crates-io].
