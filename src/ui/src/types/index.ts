// Zone types
export interface Zone {
  zone_id: string;
  display_name: string;
  state: 'playing' | 'paused' | 'stopped' | 'loading';
  now_playing?: NowPlaying;
  outputs?: Output[];
  volume?: VolumeInfo;
  source?: string;
}

export interface NowPlaying {
  one_line?: { line1: string };
  two_line?: { line1: string; line2: string };
  three_line?: { line1: string; line2: string; line3: string };
  image_key?: string;
  length?: number;
  seek_position?: number;
}

export interface Output {
  output_id: string;
  display_name: string;
  volume?: VolumeInfo;
}

export interface VolumeInfo {
  type: 'number' | 'db';
  value: number;
  min?: number;
  max?: number;
  step?: number;
  is_muted?: boolean;
}

// Knob types
export interface Knob {
  id: string;
  name?: string;
  version?: string;
  ip?: string;
  zone_id?: string;
  battery_level?: number;
  last_seen?: string;
  charging?: boolean;
}

export interface KnobConfig {
  device_name?: string;
  display_rotation?: 0 | 180;
  power_timers?: {
    charging: { art_mode: number; dim: number; sleep: number };
    battery: { art_mode: number; dim: number; sleep: number };
  };
  wifi_power_save?: boolean;
  cpu_freq_mhz?: number;
  poll_interval_ms?: number;
}

// HQPlayer types
export interface HqpStatus {
  connected: boolean;
  playing?: boolean;
  track?: string;
  artist?: string;
}

export interface HqpProfile {
  name: string;
  description?: string;
}

export interface HqpPipelineSettings {
  mode?: string;
  rate?: string;
  filter?: string;
  filter_1x?: string;
  filter_nx?: string;
  shaper?: string;
  dither?: string;
  modulator?: string;
}

export interface HqpConfig {
  host: string;
  port: number;
  web_username?: string;
  web_password?: string;
}

export interface HqpPipelineOptions {
  modes?: string[];
  rates?: string[];
  filters?: string[];
  filters_1x?: string[];
  filters_nx?: string[];
  shapers?: string[];
  dithers?: string[];
  modulators?: string[];
}

// Lyrion (LMS) types
export interface LmsStatus {
  connected: boolean;
  host?: string;
  port?: number;
  player_count?: number;
}

export interface LmsConfig {
  host: string;
  port: number;
  username?: string;
  password?: string;
}

// API response types
export interface StatusResponse {
  zones: Record<string, Zone>;
  now_playing: Record<string, NowPlaying>;
  backends: {
    roon: boolean;
    upnp: boolean;
    openhome: boolean;
    lyrion: boolean;
  };
  knobs: Knob[];
}

export interface AppSettings {
  hideKnobsPage?: boolean;
  backends?: {
    roon?: boolean;
    upnp?: boolean;
    openhome?: boolean;
    lyrion?: boolean;
  };
}

export interface FirmwareInfo {
  current_version?: string;
  available_version?: string;
  has_firmware?: boolean;
}
