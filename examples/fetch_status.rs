//! Smoke-test the LAN MQTT client against a real printer.
//!
//! Reads `BAMBU_IP`, `BAMBU_SERIAL`, `BAMBU_ACCESS_CODE` and `BAMBU_MODEL` from
//! the environment, fetches one status snapshot and prints the key fields.
//!
//! ```sh
//! BAMBU_IP=… BAMBU_SERIAL=… BAMBU_ACCESS_CODE=… BAMBU_MODEL=a1mini \
//!   cargo run --example fetch_status
//! ```

use bambu_rs::client::{LanMqttClient, StatusSource};
use bambu_rs::config::{Overrides, resolve};
use bambu_rs::core::status::PrinterStatus;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = resolve(None, &Overrides::from_env())?;
    eprintln!("connecting to {} ({})…", target.ip, target.model);

    let state = LanMqttClient::new(target).fetch_snapshot()?;
    let st = PrinterStatus::from_state(state.get());

    eprintln!("state={:?}", st.state());
    eprintln!("gcode_state={:?}", st.gcode_state);
    eprintln!("nozzle={:?}°C bed={:?}°C", st.nozzle_temper, st.bed_temper);
    eprintln!(
        "percent={:?} layer={:?}/{:?}",
        st.mc_percent, st.layer_num, st.total_layer_num
    );
    Ok(())
}
