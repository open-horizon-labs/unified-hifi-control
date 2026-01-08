import type {
  StatusResponse,
  Zone,
  Knob,
  KnobConfig,
  HqpStatus,
  HqpProfile,
  HqpPipelineSettings,
  HqpPipelineOptions,
  HqpConfig,
  LmsStatus,
  LmsConfig,
  AppSettings,
  FirmwareInfo,
} from '@/types';

const API_BASE = import.meta.env.VITE_API_BASE || '';

async function fetchJson<T>(url: string, options?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${url}`, options);
  if (!response.ok) {
    throw new Error(`API error: ${response.status} ${response.statusText}`);
  }
  return response.json();
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  return fetchJson<T>(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
}

export const api = {
  // Status
  getStatus: () => fetchJson<StatusResponse>('/admin/status.json'),

  // Zones
  getZones: () => fetchJson<Zone[]>('/roon/zones'),
  getAlbumArtUrl: (zoneId: string) => `${API_BASE}/now_playing/image?zone_id=${encodeURIComponent(zoneId)}&t=${Date.now()}`,

  // Playback Control
  sendCommand: (command: string, zoneId: string, outputId?: string) =>
    postJson<{ success: boolean }>('/control', {
      command,
      zone_id: zoneId,
      output_id: outputId,
    }),

  // HQPlayer
  getHqpStatus: () => fetchJson<HqpStatus>('/hqp/status'),
  getHqpProfiles: () => fetchJson<HqpProfile[]>('/hqp/profiles'),
  loadHqpProfile: (name: string) =>
    postJson<{ success: boolean; message?: string }>('/hqp/profiles/load', { name }),
  getHqpPipeline: () => fetchJson<{ settings: HqpPipelineSettings; options: HqpPipelineOptions }>('/hqp/pipeline'),
  updateHqpPipeline: (settings: HqpPipelineSettings) =>
    postJson<{ success: boolean }>('/hqp/pipeline', settings),
  configureHqp: (config: HqpConfig) =>
    postJson<{ success: boolean }>('/hqp/configure', config),

  // Lyrion (LMS)
  getLmsStatus: () => fetchJson<LmsStatus>('/lms/status'),
  configureLms: (config: LmsConfig) =>
    postJson<{ success: boolean }>('/lms/configure', config),

  // Knobs
  getKnobs: () => fetchJson<Knob[]>('/api/knobs'),
  getKnobConfig: (knobId: string) => fetchJson<KnobConfig>(`/config/${knobId}`),
  updateKnobConfig: (knobId: string, config: KnobConfig) =>
    fetch(`${API_BASE}/config/${knobId}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
    }).then(r => r.json()),

  // Settings
  getSettings: () => fetchJson<AppSettings>('/api/settings'),
  updateSettings: (settings: AppSettings) =>
    postJson<{ success: boolean }>('/api/settings', settings),

  // Firmware
  getFirmwareVersion: () => fetchJson<FirmwareInfo>('/firmware/version'),
  fetchFirmware: () =>
    postJson<{ success: boolean; message?: string }>('/admin/fetch-firmware', {}),

  // Debug
  getBusActivity: () => fetchJson<{ events: string[] }>('/admin/bus'),
};
