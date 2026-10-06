//! Lists audio devices with `CpalBackend`, for checking on real machines.
//!
//! Run with `cargo run -p ink-backend --example list_devices`.

use ink_backend::{AudioBackend, CpalBackend, DeviceInfo, Result};

fn main() {
    let backend = CpalBackend::new();

    println!("== Input devices");
    print_list(backend.enumerate_input_devices());
    println!("== Default input");
    print_default(backend.default_input());

    println!();
    println!("== Output devices");
    print_list(backend.enumerate_output_devices());
    println!("== Default output");
    print_default(backend.default_output());
}

fn print_list(devices: Result<Vec<DeviceInfo>>) {
    match devices {
        Ok(devices) if devices.is_empty() => println!("  (none)"),
        Ok(devices) => devices.iter().for_each(print_device),
        Err(e) => println!("  error: {e}"),
    }
}

fn print_default(device: Result<Option<DeviceInfo>>) {
    match device {
        Ok(Some(device)) => print_device(&device),
        Ok(None) => println!("  (none)"),
        Err(e) => println!("  error: {e}"),
    }
}

fn print_device(device: &DeviceInfo) {
    println!("  {}", device.name);
    println!("    id:           {}", device.id);
    println!("    channels:     {}", device.channels);
    println!("    sample rates: {:?}", device.sample_rates);
}
