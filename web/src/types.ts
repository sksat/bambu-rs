// Wire shape of the Rust `PrinterStatus` (src/core/status.rs), serialised by
// serde. Hand-written for now; P4 generates this from Rust via ts-rs so the two
// can't drift. Every field is optional because deltas and tolerant parsing mean
// any of them may be absent.
export interface PrinterStatus {
  gcode_state?: string | null;
  print_error?: number | null;
  mc_percent?: number | null;
  layer_num?: number | null;
  total_layer_num?: number | null;
  remaining_time_min?: number | null;
  stg_cur?: number | null;
  stage?: string | null;
  nozzle_temper?: number | null;
  nozzle_target?: number | null;
  bed_temper?: number | null;
  bed_target?: number | null;
  chamber_temper_raw?: number | null;
  cooling_fan_speed?: number | null;
  spd_lvl?: number | null;
  subtask_name?: string | null;
  [key: string]: unknown;
}
