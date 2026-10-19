//! Pure, shared mapping between a native zone volume and client percentages.
use crate::bus::{VolumeControl, VolumeScale};

pub fn percentage(volume: &VolumeControl) -> Option<f64> {
    if !volume.value.is_finite() {
        return None;
    }
    match volume.scale {
        VolumeScale::Percentage => Some(f64::from(volume.value)),
        VolumeScale::Linear | VolumeScale::Steps if valid_range(volume) => Some(f64::from(
            (volume.value - volume.min) / (volume.max - volume.min) * 100.0,
        )),
        _ => None,
    }
}

pub fn native(volume: &VolumeControl, percent: f64) -> Option<f64> {
    if !percent.is_finite() {
        return None;
    }
    match volume.scale {
        VolumeScale::Percentage => Some(percent),
        VolumeScale::Linear | VolumeScale::Steps if valid_range(volume) => {
            Some(f64::from(volume.min) + percent / 100.0 * f64::from(volume.max - volume.min))
        }
        _ => None,
    }
}
fn valid_range(volume: &VolumeControl) -> bool {
    volume.min.is_finite() && volume.max.is_finite() && volume.max > volume.min
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_percentage_mapping_round_trips_supported_scales() {
        for (scale, min, max, value) in [
            (VolumeScale::Percentage, 0.0, 98.0, 25.0),
            (VolumeScale::Linear, 0.0, 1.0, 0.25),
            (VolumeScale::Steps, 10.0, 170.0, 50.0),
        ] {
            let control = VolumeControl {
                value,
                min,
                max,
                step: 1.0,
                is_muted: false,
                scale,
                output_id: None,
            };
            let percent = percentage(&control).unwrap();
            assert!((native(&control, percent).unwrap() - f64::from(value)).abs() < 0.00001);
        }
    }
    #[test]
    fn logarithmic_unknown_nonfinite_and_invalid_ranges_are_not_percentages() {
        let mut control = VolumeControl {
            value: -40.0,
            min: -80.0,
            max: -10.0,
            step: 0.5,
            is_muted: false,
            scale: VolumeScale::Decibel,
            output_id: None,
        };
        for scale in [VolumeScale::Decibel, VolumeScale::Unknown] {
            control.scale = scale;
            assert!(percentage(&control).is_none());
            assert!(native(&control, 50.0).is_none());
        }
        control.scale = VolumeScale::Linear;
        control.max = control.min;
        assert!(percentage(&control).is_none());
        assert!(native(&control, 50.0).is_none());
        control.max = 0.0;
        assert!(native(&control, f64::NAN).is_none());
    }
}
