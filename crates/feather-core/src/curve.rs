//! Pure fan, color, and schedule evaluation shared by the daemon and CLI.

use crate::config::{ColorCurve, FanCurve, parse_time};

/// Interpolates a fan curve at `temperature` and returns a duty from 0 through 100.
#[must_use]
pub fn interpolate_fan(temperature: f64, curve: &FanCurve) -> u8 {
    interpolate(
        temperature,
        curve
            .points
            .iter()
            .map(|point| (point.temp, f64::from(point.percent))),
    )
    .round()
    .clamp(0.0, 100.0) as u8
}

/// Interpolates an RGB curve at `temperature`.
#[must_use]
pub fn interpolate_color(temperature: f64, curve: &ColorCurve) -> [u8; 3] {
    let Some(first) = curve.points.first() else {
        return [0, 0, 0];
    };
    let Some(last) = curve.points.last() else {
        return [0, 0, 0];
    };
    if temperature <= first.temp {
        return first.rgb;
    }
    if temperature >= last.temp {
        return last.rgb;
    }
    for pair in curve.points.windows(2) {
        let [left, right] = pair else { continue };
        if (left.temp..=right.temp).contains(&temperature) {
            let fraction = (temperature - left.temp) / (right.temp - left.temp);
            return std::array::from_fn(|index| {
                (f64::from(left.rgb[index])
                    + fraction * (f64::from(right.rgb[index]) - f64::from(left.rgb[index])))
                .round()
                .clamp(0.0, 255.0) as u8
            });
        }
    }
    last.rgb
}

fn interpolate(temperature: f64, points: impl Iterator<Item = (f64, f64)>) -> f64 {
    let points = points.collect::<Vec<_>>();
    let Some(first) = points.first() else {
        return 0.0;
    };
    let Some(last) = points.last() else {
        return 0.0;
    };
    if temperature <= first.0 {
        return first.1;
    }
    if temperature >= last.0 {
        return last.1;
    }
    for pair in points.windows(2) {
        let [left, right] = pair else { continue };
        let (left_temp, left_value) = *left;
        let (right_temp, right_value) = *right;
        if (left_temp..=right_temp).contains(&temperature) {
            let fraction = (temperature - left_temp) / (right_temp - left_temp);
            return left_value + fraction * (right_value - left_value);
        }
    }
    last.1
}

/// Returns whether a local wall-clock time falls within a configured schedule.
///
/// A schedule may cross midnight. Invalid times and equal start and end times
/// are inactive.
#[must_use]
pub fn schedule_active(hour: u32, minute: u32, start: &str, end: &str) -> bool {
    let Ok(start) = parse_time(start) else {
        return false;
    };
    let Ok(end) = parse_time(end) else {
        return false;
    };
    if hour > 23 || minute > 59 {
        return false;
    }
    let now = hour * 60 + minute;
    let start = u32::from(start.0) * 60 + u32::from(start.1);
    let end = u32::from(end.0) * 60 + u32::from(end.1);
    if start == end {
        false
    } else if start < end {
        (start..end).contains(&now)
    } else {
        now >= start || now < end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorPoint, FanPoint};

    #[test]
    fn interpolates_and_clamps_fan_curve() {
        let curve = FanCurve {
            points: vec![
                FanPoint {
                    temp: 40.0,
                    percent: 30,
                },
                FanPoint {
                    temp: 80.0,
                    percent: 100,
                },
            ],
        };
        assert_eq!(interpolate_fan(20.0, &curve), 30);
        assert_eq!(interpolate_fan(60.0, &curve), 65);
        assert_eq!(interpolate_fan(90.0, &curve), 100);
    }

    #[test]
    fn interpolates_color_channels() {
        let curve = ColorCurve {
            points: vec![
                ColorPoint {
                    temp: 40.0,
                    rgb: [0, 80, 0],
                },
                ColorPoint {
                    temp: 60.0,
                    rgb: [200, 0, 0],
                },
            ],
        };
        assert_eq!(interpolate_color(50.0, &curve), [100, 40, 0]);
    }

    #[test]
    fn handles_night_windows() {
        assert!(schedule_active(23, 0, "22:00", "07:00"));
        assert!(schedule_active(6, 59, "22:00", "07:00"));
        assert!(!schedule_active(12, 0, "22:00", "07:00"));
        assert!(!schedule_active(22, 0, "22:00", "22:00"));
    }
}
