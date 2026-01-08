const mqtt = require('mqtt');

const DEFAULT_TOPIC_PREFIX = 'unified-hifi';
const PUBLISH_INTERVAL_MS = 5000; // Publish state every 5 seconds when connected

function createMqttService({ hqp, firmware, logger } = {}) {
  const log = logger || console;
  let client = null;
  let publishTimer = null;
  let topicPrefix = DEFAULT_TOPIC_PREFIX;
  let firmwareUnsubscribe = null;

  function isEnabled() {
    return !!process.env.MQTT_BROKER;
  }

  function connect() {
    if (!isEnabled()) {
      log.info('MQTT disabled (set MQTT_BROKER to enable)');
      return;
    }

    const broker = process.env.MQTT_BROKER;
    topicPrefix = process.env.MQTT_TOPIC_PREFIX || DEFAULT_TOPIC_PREFIX;

    const options = {
      clientId: `unified-hifi-${Date.now()}`,
      clean: true,
      reconnectPeriod: 5000,
    };

    if (process.env.MQTT_USER) {
      options.username = process.env.MQTT_USER;
      options.password = process.env.MQTT_PASSWORD || process.env.MQTT_PASS || '';
    }

    // Add protocol if missing
    const brokerUrl = broker.includes('://') ? broker : `mqtt://${broker}`;
    log.info('Connecting to MQTT broker', { broker: brokerUrl, topicPrefix });

    client = mqtt.connect(brokerUrl, options);

    client.on('connect', async () => {
      log.info('MQTT connected');

      // Subscribe to command topics
      const commandTopics = [
        `${topicPrefix}/command/hqplayer/load`,
        `${topicPrefix}/command/hqplayer/pipeline`,
        `${topicPrefix}/hqplayer/filter1x/set`,
        `${topicPrefix}/hqplayer/shaper/set`,
        `${topicPrefix}/hqplayer/samplerate/set`,
        `${topicPrefix}/hqplayer/mode/set`,
        `${topicPrefix}/hqplayer/profile/set`,
        `${topicPrefix}/hqplayer/volume/set`,
      ];

      commandTopics.forEach(topic => {
        client.subscribe(topic, (err) => {
          if (err) {
            log.error('MQTT subscribe failed', { topic, error: err.message });
          } else {
            log.info('MQTT subscribed', { topic });
          }
        });
      });

      // Start publishing state
      startPublishing();

      // Publish discovery config for Home Assistant (includes selects with options)
      await publishHADiscovery();

      // Subscribe to firmware events
      if (firmware) {
        firmwareUnsubscribe = firmware.on('firmware_downloaded', (data) => {
          publishFirmwareUpdate(data);
        });
        // Publish current firmware status
        publishFirmwareStatus();
      }
    });

    client.on('error', (err) => {
      log.error('MQTT error', { error: err.message });
    });

    client.on('reconnect', () => {
      log.info('MQTT reconnecting...');
    });

    client.on('message', async (topic, message) => {
      try {
        await handleMessage(topic, message.toString());
      } catch (err) {
        log.error('MQTT message handler error', { topic, error: err.message });
      }
    });
  }

  async function handleMessage(topic, payload) {
    log.debug('MQTT message received', { topic, payload });

    if (!hqp.isConfigured()) {
      log.warn('HQPlayer not configured, ignoring command');
      return;
    }

    if (topic === `${topicPrefix}/command/hqplayer/load`) {
      if (!hqp.hasWebCredentials()) {
        log.warn('Profile loading requires web credentials');
        return;
      }
      const profile = payload.trim();
      if (profile) {
        log.info('Loading HQPlayer profile via MQTT', { profile });
        await hqp.loadProfile(profile);
        setTimeout(() => publishHqpState(), 10000);
      }
    } else if (topic === `${topicPrefix}/command/hqplayer/pipeline`) {
      try {
        const { setting, value } = JSON.parse(payload);
        if (setting && value !== undefined) {
          log.info('Setting HQPlayer pipeline via MQTT', { setting, value });
          await hqp.setPipelineSetting(setting, value);
          setTimeout(() => publishHqpState(), 1000);
        }
      } catch (e) {
        log.warn('Invalid pipeline command payload', { payload, error: e.message });
      }
    } else if (topic === `${topicPrefix}/hqplayer/filter1x/set`) {
      // Map label back to value
      const pipeline = await hqp.fetchPipeline();
      const opt = pipeline.settings?.filter1x?.options?.find(o => o.label === payload);
      const value = opt?.value || payload;
      log.info('Setting HQPlayer filter via MQTT select', { label: payload, value });
      await hqp.setPipelineSetting('filter1x', value);
      setTimeout(() => publishHqpState(), 1000);
    } else if (topic === `${topicPrefix}/hqplayer/shaper/set`) {
      const pipeline = await hqp.fetchPipeline();
      const opt = pipeline.settings?.shaper?.options?.find(o => o.label === payload);
      const value = opt?.value || payload;
      log.info('Setting HQPlayer shaper via MQTT select', { label: payload, value });
      await hqp.setPipelineSetting('shaper', value);
      setTimeout(() => publishHqpState(), 1000);
    } else if (topic === `${topicPrefix}/hqplayer/samplerate/set`) {
      const pipeline = await hqp.fetchPipeline();
      const opt = pipeline.settings?.samplerate?.options?.find(o => o.label === payload);
      const value = opt?.value || payload;
      log.info('Setting HQPlayer samplerate via MQTT select', { label: payload, value });
      await hqp.setPipelineSetting('samplerate', value);
      setTimeout(() => publishHqpState(), 1000);
    } else if (topic === `${topicPrefix}/hqplayer/mode/set`) {
      const pipeline = await hqp.fetchPipeline();
      const opt = pipeline.settings?.mode?.options?.find(o => o.label === payload);
      const value = opt?.value || payload;
      log.info('Setting HQPlayer mode via MQTT select', { label: payload, value });
      await hqp.setPipelineSetting('mode', value);
      setTimeout(() => publishHqpState(), 1000);
    } else if (topic === `${topicPrefix}/hqplayer/profile/set`) {
      if (!hqp.hasWebCredentials()) {
        log.warn('Profile loading requires web credentials');
        return;
      }
      log.info('Loading HQPlayer profile via MQTT select', { profile: payload });
      await hqp.loadProfile(payload);
      // HQPlayer restarts when loading profile - wait longer
      setTimeout(() => publishHqpState(), 10000);
    } else if (topic === `${topicPrefix}/hqplayer/volume/set`) {
      const value = parseFloat(payload);
      if (!isNaN(value)) {
        log.info('Setting HQPlayer volume via MQTT', { value });
        await hqp.setVolume(value);
        setTimeout(() => publishHqpState(), 500);
      }
    }
  }

  function startPublishing() {
    if (publishTimer) {
      clearInterval(publishTimer);
    }

    // Publish immediately
    publishHqpState();

    // Then publish periodically
    publishTimer = setInterval(() => {
      publishHqpState();
    }, PUBLISH_INTERVAL_MS);
  }

  async function publishHqpState() {
    if (!client || !client.connected) return;
    if (!hqp) {
      log.debug('HQPlayer not configured, skipping MQTT publish');
      return;
    }

    try {
      const status = await hqp.getStatus();

      // Publish status
      client.publish(
        `${topicPrefix}/hqplayer/status`,
        JSON.stringify(status),
        { retain: true }
      );

      // If connected, publish detailed pipeline and update select states
      if (status.connected && status.pipeline) {
        client.publish(
          `${topicPrefix}/hqplayer/pipeline`,
          JSON.stringify(status.pipeline),
          { retain: true }
        );

        // Keep select entity states in sync with actual HQPlayer state
        const settings = status.pipeline.settings || {};
        if (settings.filter1x?.selected?.label) {
          client.publish(
            `${topicPrefix}/hqplayer/filter1x/state`,
            settings.filter1x.selected.label,
            { retain: true }
          );
        }
        if (settings.shaper?.selected?.label) {
          client.publish(
            `${topicPrefix}/hqplayer/shaper/state`,
            settings.shaper.selected.label,
            { retain: true }
          );
        }
        if (settings.samplerate?.selected?.label) {
          client.publish(
            `${topicPrefix}/hqplayer/samplerate/state`,
            settings.samplerate.selected.label,
            { retain: true }
          );
        }
        if (settings.mode?.selected?.label) {
          client.publish(
            `${topicPrefix}/hqplayer/mode/state`,
            settings.mode.selected.label,
            { retain: true }
          );
        }

        // Sync volume state
        if (status.pipeline.volume?.value !== undefined) {
          client.publish(
            `${topicPrefix}/hqplayer/volume/state`,
            String(status.pipeline.volume.value),
            { retain: true }
          );
        }
      }

      // Sync profile state using configName as best available proxy
      // HQPlayer doesn't expose "active profile" directly, but configName
      // typically reflects the loaded profile after a profile switch
      if (status.configName) {
        client.publish(
          `${topicPrefix}/hqplayer/profile/state`,
          status.configName,
          { retain: true }
        );
      }

      // Publish profiles list
      if (status.profiles && status.profiles.length > 0) {
        client.publish(
          `${topicPrefix}/hqplayer/profiles`,
          JSON.stringify(status.profiles),
          { retain: true }
        );
      }

      log.debug('Published HQPlayer state to MQTT');
    } catch (err) {
      log.warn('Failed to publish HQPlayer state', { error: err.message });
    }
  }

  async function publishHADiscovery() {
    if (!client || !client.connected) return;

    // Fetch pipeline and profiles to get available options for selects
    let pipelineSettings = {};
    let pipelineVolume = null;
    let profiles = [];
    let supportsProfiles = false;
    if (hqp.isConfigured()) {
      try {
        const pipeline = await hqp.fetchPipeline();
        pipelineSettings = pipeline.settings || {};
        pipelineVolume = pipeline.volume || null;
      } catch (err) {
        log.warn('Failed to fetch pipeline for discovery', { error: err.message });
      }
      // Only fetch profiles if we have web credentials (Embedded only)
      if (hqp.hasWebCredentials()) {
        try {
          profiles = await hqp.fetchProfiles();
          supportsProfiles = profiles.length > 0;
        } catch (err) {
          log.warn('Failed to fetch profiles for discovery', { error: err.message });
        }
      }
    }

    // HQPlayer config name sensor
    const configSensor = {
      name: 'HQPlayer Config',
      unique_id: 'unified_hifi_hqp_config',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.configName | default("Unknown", true) }}',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:audio-video',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_config/config',
      JSON.stringify(configSensor),
      { retain: true }
    );

    // HQPlayer state sensor (playing/stopped)
    const stateSensor = {
      name: 'HQPlayer State',
      unique_id: 'unified_hifi_hqp_state',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.pipeline.status.state | default("Unknown", true) }}',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:play-circle',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_state/config',
      JSON.stringify(stateSensor),
      { retain: true }
    );

    // HQPlayer volume sensor
    const volumeSensor = {
      name: 'HQPlayer Volume',
      unique_id: 'unified_hifi_hqp_volume',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.pipeline.volume.value | default(0) }}',
      unit_of_measurement: 'dB',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:volume-high',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_volume/config',
      JSON.stringify(volumeSensor),
      { retain: true }
    );

    // HQPlayer volume control (number entity) - only if volume is variable
    if (pipelineVolume && !pipelineVolume.isFixed) {
      const volumeNumber = {
        name: 'HQPlayer Volume Control',
        unique_id: 'unified_hifi_hqp_volume_control',
        state_topic: `${topicPrefix}/hqplayer/volume/state`,
        command_topic: `${topicPrefix}/hqplayer/volume/set`,
        min: pipelineVolume.min,
        max: pipelineVolume.max,
        step: 1,
        mode: 'slider',
        unit_of_measurement: 'dB',
        availability_topic: `${topicPrefix}/hqplayer/status`,
        availability_template: '{{ "online" if value_json.connected else "offline" }}',
        icon: 'mdi:volume-high',
      };
      client.publish(
        'homeassistant/number/unified_hifi_hqp_volume/config',
        JSON.stringify(volumeNumber),
        { retain: true }
      );

      // Publish current volume state
      client.publish(
        `${topicPrefix}/hqplayer/volume/state`,
        String(pipelineVolume.value),
        { retain: true }
      );
    }

    // HQPlayer filter sensor
    const filterSensor = {
      name: 'HQPlayer Filter',
      unique_id: 'unified_hifi_hqp_filter',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.pipeline.settings.filter1x.selected.label | default("Unknown", true) }}',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:tune',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_filter/config',
      JSON.stringify(filterSensor),
      { retain: true }
    );

    // HQPlayer sample rate sensor
    const samplerateSensor = {
      name: 'HQPlayer Sample Rate',
      unique_id: 'unified_hifi_hqp_samplerate',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.pipeline.settings.samplerate.selected.label | default("Unknown", true) }}',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:waveform',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_samplerate/config',
      JSON.stringify(samplerateSensor),
      { retain: true }
    );

    // HQPlayer dither sensor
    const ditherSensor = {
      name: 'HQPlayer Dither',
      unique_id: 'unified_hifi_hqp_dither',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.pipeline.settings.dither.selected.label | default("Unknown", true) }}',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:sine-wave',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_dither/config',
      JSON.stringify(ditherSensor),
      { retain: true }
    );

    // HQPlayer mode sensor (PCM/SDM)
    const modeSensor = {
      name: 'HQPlayer Mode',
      unique_id: 'unified_hifi_hqp_mode',
      state_topic: `${topicPrefix}/hqplayer/status`,
      value_template: '{{ value_json.pipeline.settings.mode.selected.label | default("Unknown", true) }}',
      availability_topic: `${topicPrefix}/hqplayer/status`,
      availability_template: '{{ "online" if value_json.connected else "offline" }}',
      icon: 'mdi:swap-horizontal',
    };

    client.publish(
      'homeassistant/sensor/unified_hifi_hqp_mode/config',
      JSON.stringify(modeSensor),
      { retain: true }
    );

    // Select entities for control (only if we have options)
    if (pipelineSettings.filter1x && pipelineSettings.filter1x.options.length > 0) {
      const filterSelect = {
        name: 'HQPlayer Filter Select',
        unique_id: 'unified_hifi_hqp_filter_select',
        state_topic: `${topicPrefix}/hqplayer/filter1x/state`,
        command_topic: `${topicPrefix}/hqplayer/filter1x/set`,
        options: pipelineSettings.filter1x.options.map(o => o.label),
        availability_topic: `${topicPrefix}/hqplayer/status`,
        availability_template: '{{ "online" if value_json.connected else "offline" }}',
        icon: 'mdi:tune',
      };
      client.publish(
        'homeassistant/select/unified_hifi_hqp_filter/config',
        JSON.stringify(filterSelect),
        { retain: true }
      );

      // Publish current state (use label to match options)
      client.publish(
        `${topicPrefix}/hqplayer/filter1x/state`,
        pipelineSettings.filter1x.selected?.label || '',
        { retain: true }
      );
    }

    if (pipelineSettings.shaper && pipelineSettings.shaper.options.length > 0) {
      const shaperSelect = {
        name: 'HQPlayer Shaper Select',
        unique_id: 'unified_hifi_hqp_shaper_select',
        state_topic: `${topicPrefix}/hqplayer/shaper/state`,
        command_topic: `${topicPrefix}/hqplayer/shaper/set`,
        options: pipelineSettings.shaper.options.map(o => o.label),
        availability_topic: `${topicPrefix}/hqplayer/status`,
        availability_template: '{{ "online" if value_json.connected else "offline" }}',
        icon: 'mdi:wave',
      };
      client.publish(
        'homeassistant/select/unified_hifi_hqp_shaper/config',
        JSON.stringify(shaperSelect),
        { retain: true }
      );

      client.publish(
        `${topicPrefix}/hqplayer/shaper/state`,
        pipelineSettings.shaper.selected?.label || '',
        { retain: true }
      );
    }

    if (pipelineSettings.samplerate && pipelineSettings.samplerate.options.length > 0) {
      const samplerateSelect = {
        name: 'HQPlayer Sample Rate Select',
        unique_id: 'unified_hifi_hqp_samplerate_select',
        state_topic: `${topicPrefix}/hqplayer/samplerate/state`,
        command_topic: `${topicPrefix}/hqplayer/samplerate/set`,
        options: pipelineSettings.samplerate.options.map(o => o.label),
        availability_topic: `${topicPrefix}/hqplayer/status`,
        availability_template: '{{ "online" if value_json.connected else "offline" }}',
        icon: 'mdi:waveform',
      };
      client.publish(
        'homeassistant/select/unified_hifi_hqp_samplerate/config',
        JSON.stringify(samplerateSelect),
        { retain: true }
      );

      client.publish(
        `${topicPrefix}/hqplayer/samplerate/state`,
        pipelineSettings.samplerate.selected?.label || '',
        { retain: true }
      );
    }

    if (pipelineSettings.mode && pipelineSettings.mode.options.length > 0) {
      const modeSelect = {
        name: 'HQPlayer Mode Select',
        unique_id: 'unified_hifi_hqp_mode_select',
        state_topic: `${topicPrefix}/hqplayer/mode/state`,
        command_topic: `${topicPrefix}/hqplayer/mode/set`,
        options: pipelineSettings.mode.options.map(o => o.label),
        availability_topic: `${topicPrefix}/hqplayer/status`,
        availability_template: '{{ "online" if value_json.connected else "offline" }}',
        icon: 'mdi:swap-horizontal',
      };
      client.publish(
        'homeassistant/select/unified_hifi_hqp_mode/config',
        JSON.stringify(modeSelect),
        { retain: true }
      );

      client.publish(
        `${topicPrefix}/hqplayer/mode/state`,
        pipelineSettings.mode.selected?.label || '',
        { retain: true }
      );
    }

    // Configuration select - uses configName as the active configuration indicator
    // HQPlayer doesn't explicitly expose "active configuration" but configName
    // typically matches the loaded configuration name after switching.
    // Recommendation: Set your HQPlayer config title to match your configuration name
    // for accurate state sync in Home Assistant.
    // Only show configuration select if HQPlayer Embedded with web credentials
    if (supportsProfiles) {
      const profileSelect = {
        name: 'HQPlayer Configuration',
        unique_id: 'unified_hifi_hqp_profile_select',
        state_topic: `${topicPrefix}/hqplayer/profile/state`,
        command_topic: `${topicPrefix}/hqplayer/profile/set`,
        options: profiles.map(p => p.value),
        availability_topic: `${topicPrefix}/hqplayer/status`,
        availability_template: '{{ "online" if value_json.connected else "offline" }}',
        icon: 'mdi:playlist-music',
      };
      client.publish(
        'homeassistant/select/unified_hifi_hqp_profile/config',
        JSON.stringify(profileSelect),
        { retain: true }
      );
      // State is synced by publishHqpState() using configName
    }

    // Firmware version sensor
    if (firmware) {
      const firmwareSensor = {
        name: 'Knob Firmware Version',
        unique_id: 'unified_hifi_knob_firmware',
        state_topic: `${topicPrefix}/firmware/version`,
        icon: 'mdi:chip',
      };

      client.publish(
        'homeassistant/sensor/unified_hifi_knob_firmware/config',
        JSON.stringify(firmwareSensor),
        { retain: true }
      );
    }

    log.info('Published Home Assistant MQTT discovery configs');
  }

  function publishFirmwareStatus() {
    if (!client || !client.connected || !firmware) return;

    const status = firmware.getStatus();
    client.publish(
      `${topicPrefix}/firmware/status`,
      JSON.stringify(status),
      { retain: true }
    );

    if (status.currentVersion) {
      client.publish(
        `${topicPrefix}/firmware/version`,
        status.currentVersion,
        { retain: true }
      );
    }

    log.debug('Published firmware status to MQTT', { version: status.currentVersion });
  }

  function publishFirmwareUpdate(data) {
    if (!client || !client.connected) return;

    client.publish(
      `${topicPrefix}/firmware/available`,
      JSON.stringify({
        version: data.version,
        size: data.size,
        releaseUrl: data.releaseUrl,
        timestamp: Date.now()
      }),
      { retain: true }
    );

    // Also update the version topic
    client.publish(
      `${topicPrefix}/firmware/version`,
      data.version,
      { retain: true }
    );

    log.info('Published firmware update to MQTT', { version: data.version });
  }

  function disconnect() {
    if (publishTimer) {
      clearInterval(publishTimer);
      publishTimer = null;
    }
    if (firmwareUnsubscribe) {
      firmwareUnsubscribe();
      firmwareUnsubscribe = null;
    }
    if (client) {
      client.end();
      client = null;
    }
  }

  return {
    isEnabled,
    connect,
    disconnect,
    publishHqpState,
  };
}

module.exports = { createMqttService };
