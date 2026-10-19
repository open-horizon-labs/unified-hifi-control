mod linked_volume {
    use super::*;
    use crate::bus::{BusEvent, VolumeScale};
    use crate::devices::{
        links::DeviceLinkStore,
        service::{DeviceDriver, DeviceOperation, DeviceService},
        CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
        DeviceConnection, DeviceId, DeviceLink, DevicePower, DeviceVolume,
    };
    use std::sync::Mutex;

    struct Amplifier {
        device: Mutex<Device>,
        operations: Mutex<Vec<DeviceOperation>>,
        bus: crate::bus::SharedBus,
        confirm: bool,
    }

    #[async_trait::async_trait]
    impl DeviceDriver for Amplifier {
        fn kinds(&self) -> &'static [&'static str] {
            &["marantz"]
        }
        async fn execute(&self, id: &DeviceId, operation: DeviceOperation) -> Result<(), String> {
            let mut device = self.device.lock().unwrap();
            assert_eq!(id, &device.device_id);
            self.operations.lock().unwrap().push(operation.clone());
            if self.confirm {
                if let DeviceOperation::SetVolume(value) = operation {
                    device.volume.value = Some(value);
                }
                device.revision += 1;
                self.bus.publish(BusEvent::DeviceObserved {
                    device: Box::new(device.clone()),
                });
            }
            Ok(())
        }
    }

    async fn exercise(confirm: bool) {
        let mut state = empty_app_state().await;
        let provider = Arc::new(RecordingSpotifyAdapter::default());
        state.adapter_registry.register(provider.clone()).await;
        let (ready, received) = tokio::sync::oneshot::channel();
        let aggregator = state.aggregator.clone();
        let task = tokio::spawn(async move { aggregator.run_with_ready(ready).await });
        received.await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        state.devices = Arc::new(
            DeviceService::new(
                state.aggregator.clone(),
                state.bus.clone(),
                DeviceLinkStore::open(dir.path().join("links.json")),
            )
            .with_confirm_timeout(std::time::Duration::from_millis(100)),
        );
        let capability = DeviceCapability {
            supported: true,
            evidence: CapabilityEvidence::ImplementationDerived,
            detail: String::new(),
        };
        let device = Device {
            device_id: DeviceId::new("marantz", "FAKE123").unwrap(),
            vendor: "Marantz".into(),
            model: "Test receiver".into(),
            name: "Front".into(),
            host: "192.0.2.1".into(),
            discovery: "manual".into(),
            maturity: "alpha".into(),
            connection: DeviceConnection {
                control_channel: ControlChannel::Push,
                detail: None,
            },
            power: DevicePower::On,
            input: None,
            inputs: vec![],
            volume: DeviceVolume {
                scale: VolumeScale::Decibel,
                value: Some(-45.0),
                min: -80.0,
                hardware_max: Some(0.0),
                ceiling: None,
                step: 0.5,
                is_muted: Some(false),
                display: None,
            },
            capabilities: DeviceCapabilities {
                observe: capability.clone(),
                volume: capability.clone(),
                mute: capability.clone(),
                power: capability.clone(),
                input_select: capability,
            },
            revision: 1,
            last_updated: 0,
        };
        let amp = Arc::new(Amplifier {
            device: Mutex::new(device.clone()),
            operations: Mutex::new(vec![]),
            bus: state.bus.clone(),
            confirm,
        });
        state.devices.register_driver(amp.clone()).await;
        state.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device.clone()),
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while state
                .aggregator
                .get_device(&device.device_id)
                .await
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state
            .aggregator
            .install_device_configuration(
                vec![DeviceLink {
                    zone_id: "spotify:fake-streamer".into(),
                    device_id: device.device_id.clone(),
                    source_id: None,
                }],
                [(device.device_id.clone(), -10.0)].into(),
            )
            .await;
        let mut store = StateStore::default();
        let projection = store
            .snapshot(SemanticStateInput {
                installation_id: Uuid::new_v4().to_string(),
                epoch: 7,
                revision: 1,
                observed_at: super::super::now_ms() as u64,
                expires_at: (super::super::now_ms() + 30_000) as u64,
                zones: vec![SemanticZoneInput {
                    provider_id: "spotify:fake-streamer".into(),
                    name: "Front".into(),
                    state: "playing".into(),
                    control: ControlEligibility::all(),
                    volume: Some(crate::cloud_connector::state::VolumeInput {
                        value: -45.0,
                        min: -80.0,
                        max: -10.0,
                        step: 0.5,
                        scale: "db".into(),
                    }),
                    now_playing: None,
                }],
            })
            .unwrap();
        let handle = projection.zones[0].zone_handle.clone();
        for (action, expected) in [
            (CommandAction::VolumeAbsolute { value: -44.5 }, -44.5),
            (CommandAction::VolumeUp, -44.0),
            (CommandAction::VolumeDown, -44.5),
        ] {
            let outcome = super::super::dispatch(
                &state,
                &store,
                &CommandPayload {
                    zone_handle: handle.clone(),
                    action,
                },
            )
            .await;
            assert_eq!(
                outcome,
                if confirm {
                    super::super::CommandOutcome::Executed
                } else {
                    super::super::CommandOutcome::UnknownOutcome
                }
            );
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                0,
                "linked volume must never reach the transport provider"
            );
            if confirm {
                assert_eq!(
                    amp.operations.lock().unwrap().last(),
                    Some(&DeviceOperation::SetVolume(expected))
                );
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while state
                        .aggregator
                        .get_device(&device.device_id)
                        .await
                        .unwrap()
                        .volume
                        .value
                        != Some(expected)
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
        }
        if confirm {
            // A ceiling changed since the cloud snapshot must still bound the write.
            state
                .aggregator
                .install_device_configuration(
                    vec![DeviceLink {
                        zone_id: "spotify:fake-streamer".into(),
                        device_id: device.device_id.clone(),
                        source_id: None,
                    }],
                    [(device.device_id.clone(), -44.5)].into(),
                )
                .await;
            assert_eq!(
                super::super::dispatch(
                    &state,
                    &store,
                    &CommandPayload {
                        zone_handle: handle.clone(),
                        action: CommandAction::VolumeAbsolute { value: -40.0 }
                    }
                )
                .await,
                super::super::CommandOutcome::Executed
            );
            assert_eq!(
                amp.operations.lock().unwrap().last(),
                Some(&DeviceOperation::SetVolume(-44.5))
            );
        }
        if confirm {
            // MQTT percentages are inverted only for scales that publish a
            // linear normalization. They must never become dB or be rounded
            // through the transport adapter's integer convention.
            state
                .aggregator
                .install_device_configuration(
                    vec![DeviceLink {
                        zone_id: "spotify:fake-streamer".into(),
                        device_id: device.device_id.clone(),
                        source_id: None,
                    }],
                    Default::default(),
                )
                .await;
            for (scale, max, step, expected) in [
                (VolumeScale::Percentage, 100.0, 1.0, 25.0),
                (VolumeScale::Linear, 1.0, 0.01, 0.25),
                (VolumeScale::Steps, 160.0, 1.0, 40.0),
            ] {
                let update = {
                    let mut device = amp.device.lock().unwrap();
                    device.volume.scale = scale;
                    device.volume.min = 0.0;
                    device.volume.hardware_max = Some(max);
                    device.volume.value = Some(0.0);
                    device.volume.step = step;
                    device.revision += 1;
                    device.clone()
                };
                let revision = update.revision;
                state.bus.publish(BusEvent::DeviceObserved {
                    device: Box::new(update),
                });
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while state
                        .aggregator
                        .get_device(&device.device_id)
                        .await
                        .unwrap()
                        .revision
                        < revision
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(
                    crate::zone_control::dispatch(
                        &state.adapter_registry,
                        &state.aggregator,
                        &state.devices,
                        None,
                        "spotify:fake-streamer",
                        crate::zone_control::Action::Volume(25.0),
                    )
                    .await,
                    crate::zone_control::DispatchOutcome::Sent
                );
                assert_eq!(
                    amp.operations.lock().unwrap().last(),
                    Some(&DeviceOperation::SetVolume(expected))
                );
            }
        }
        // A missing driver cannot fall back to the provider, even when its
        // registry would accept the command and report success.
        state.devices = Arc::new(DeviceService::new(
            state.aggregator.clone(),
            state.bus.clone(),
            DeviceLinkStore::open(dir.path().join("no-driver.json")),
        ));
        assert_eq!(
            super::super::dispatch(
                &state,
                &store,
                &CommandPayload {
                    zone_handle: handle.clone(),
                    action: CommandAction::VolumeAbsolute { value: -45.0 }
                }
            )
            .await,
            super::super::CommandOutcome::Rejected
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            super::super::dispatch(
                &state,
                &store,
                &CommandPayload {
                    zone_handle: handle,
                    action: CommandAction::Next
                }
            )
            .await,
            super::super::CommandOutcome::Executed
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "transport stays with its provider"
        );
        task.abort();
    }

    #[tokio::test]
    async fn hiphi_linked_volume_uses_amplifier_native_steps() {
        exercise(true).await;
    }

    #[tokio::test]
    async fn hiphi_linked_volume_without_readback_is_unknown() {
        exercise(false).await;
    }
}
