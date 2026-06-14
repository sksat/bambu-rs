// Wire shape of the Rust `PrinterStatus` (src/core/status.rs), serialised by
// serde. Hand-written for now; a follow-up generates this from Rust via ts-rs so
// the two can't drift. Most fields are optional: deltas, tolerant parsing, and
// `#[serde(skip_serializing_if)]` mean any of them may be absent.
export interface PrinterStatus {
  gcode_state?: string | null;
  print_error?: number | null;
  error?: DeviceError | null;
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
  // chamber_temper_raw is intentionally not rendered: it's synthetic on A1/P1.

  cooling_fan_speed?: number | null;
  big_fan1_speed?: number | null;
  big_fan2_speed?: number | null;
  heatbreak_fan_speed?: number | null;

  spd_lvl?: number | null;
  spd_mag?: number | null;
  subtask_name?: string | null;
  gcode_file?: string | null;
  gcode_file_prepare_percent?: number | null;
  print_type?: string | null;

  nozzle_diameter?: string | null;
  nozzle_type?: string | null;
  sdcard?: boolean | null;
  wifi_signal?: string | null;

  filament?: Filament | null;
  ams?: Ams | null;
  hms?: HmsAlert[];
  upgrade?: Upgrade | null;
  online?: Online | null;
  upload?: Upload | null;
  lights?: LightReport[];
  ipcam?: Ipcam | null;
}

export interface DeviceError {
  code: number;
  hex: string;
  lookup_url: string;
}

export interface Filament {
  location: string;
  material?: string | null;
  name?: string | null;
  color?: string | null;
}

export interface HmsAlert {
  code: string;
  code_hyphen: string;
  severity: number;
  module: string;
  is_lidar: boolean;
  wiki: string;
  attr: number;
  raw_code: number;
}

export interface Ams {
  units?: AmsUnit[];
  external?: AmsTray | null;
  active_tray?: string | null;
  target_tray?: string | null;
  previous_tray?: string | null;
  ams_exist_bits?: string | null;
  tray_exist_bits?: string | null;
  tray_is_bbl_bits?: string | null;
}

export interface AmsUnit {
  id: string;
  humidity?: number | null;
  humidity_raw?: number | null;
  temp?: number | null;
  dry_time?: number | null;
  trays?: AmsTray[];
}

export interface AmsTray {
  id: string;
  material?: string | null;
  name?: string | null;
  color?: string | null;
  cols?: string[];
  remain?: number | null;
  state?: number | null;
  info_idx?: string | null;
  id_name?: string | null;
  uuid?: string | null;
  nozzle_temp_min?: number | null;
  nozzle_temp_max?: number | null;
  is_active: boolean;
  is_target: boolean;
}

export interface Upgrade {
  new_version_state?: number | null;
  cur_state_code?: number | null;
  status?: string | null;
  err_code?: number | null;
}

export interface Online {
  ahb?: boolean | null;
  rfid?: boolean | null;
  version?: number | null;
}

export interface Upload {
  status?: string | null;
  progress?: number | null;
  message?: string | null;
}

export interface LightReport {
  node: string;
  mode: string;
}

export interface Ipcam {
  timelapse?: string | null;
  record?: string | null;
  resolution?: string | null;
}
