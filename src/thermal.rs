// SPDX-License-Identifier: GPL-3.0-or-later

//! Temperature maths shared by the CLI, the HTTP API and the viewer.

use std::fmt::Write;

use crate::camera::{Frame, KELVIN, Result, celsius};

/// Turns the camera's apparent temperature into the object's.
///
/// The camera reports what a perfect black body would have to be at to emit
/// what it sees. A real surface emits less and reflects some of its
/// surroundings, so the reading is corrected with the Stefan-Boltzmann law:
/// `T_obj^4 = (T_app^4 - (1 - e) * T_refl^4) / e`. The sensor only sees 8-14 µm
/// rather than all wavelengths, so this is an approximation; it is the same
/// one the reference driver uses.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "gui", derive(serde::Serialize, serde::Deserialize))]
pub struct Correction {
    /// 0.95 suits skin, paint, wood, plastics and oxidised metal; bare metal
    /// needs much less, and below ~0.5 readings become unreliable.
    pub emissivity: f32,
    /// Apparent temperature of what the surface reflects, in °C; usually the room.
    pub reflected: f32,
    /// Added to every reading, in °C. The calibration knob: point the camera at
    /// something of known temperature and dial out the difference.
    pub offset: f32,
}

impl Correction {
    pub const DEFAULT: Self = Self {
        emissivity: 0.95,
        reflected: 20.0,
        offset: 0.0,
    };

    pub fn celsius(&self, raw: u16) -> f32 {
        let e = self.emissivity.clamp(0.01, 1.0);
        if e >= 1.0 {
            return celsius(raw) + self.offset;
        }
        let apparent = (raw as f32 / 64.0).powi(4);
        let reflected = (self.reflected + KELVIN).powi(4);
        let object = ((apparent - (1.0 - e) * reflected) / e).max(0.0).powf(0.25);
        object - KELVIN + self.offset
    }
}

impl Default for Correction {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Corrected temperatures in °C, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub temps: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    /// Temperature and pixel index.
    pub min: (f32, usize),
    pub max: (f32, usize),
    pub mean: f32,
}

impl Image {
    pub fn new(frame: &Frame, correction: &Correction) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            temps: frame.raw.iter().map(|&r| correction.celsius(r)).collect(),
        }
    }

    pub fn at(&self, x: usize, y: usize) -> Option<f32> {
        (x < self.width && y < self.height).then(|| self.temps[y * self.width + x])
    }

    pub fn center(&self) -> f32 {
        self.temps[self.height / 2 * self.width + self.width / 2]
    }

    pub fn stats(&self) -> Stats {
        let mut min = (f32::INFINITY, 0);
        let mut max = (f32::NEG_INFINITY, 0);
        for (i, &t) in self.temps.iter().enumerate() {
            if t < min.0 {
                min = (t, i);
            }
            if t > max.0 {
                max = (t, i);
            }
        }
        let mean = self.temps.iter().sum::<f32>() / self.temps.len() as f32;
        Stats { min, max, mean }
    }

    /// Plain-text readout for the CLI and the HTTP API, one `name value` per line.
    pub fn report(&self, point: Option<(usize, usize)>) -> Result<String> {
        let stats = self.stats();
        let mut out = String::new();
        for (name, t) in [
            ("min", stats.min.0),
            ("max", stats.max.0),
            ("mean", stats.mean),
            ("center", self.center()),
        ] {
            let _ = writeln!(out, "{name}\t{t:.2}");
        }
        if let Some((x, y)) = point {
            let t = self.at(x, y).ok_or_else(|| {
                format!(
                    "{x},{y} is outside the {}x{} image",
                    self.width, self.height
                )
            })?;
            let _ = writeln!(out, "point\t{t:.2}");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(c: f32) -> u16 {
        ((c + KELVIN) * 64.0).round() as u16
    }

    #[test]
    fn emissivity_one_reads_the_camera_directly() {
        let c = Correction {
            emissivity: 1.0,
            reflected: 20.0,
            offset: 0.0,
        };
        assert!((c.celsius(raw(36.6)) - 36.6).abs() < 0.02);
        let c = Correction { offset: 1.5, ..c };
        assert!((c.celsius(raw(36.6)) - 38.1).abs() < 0.02);
    }

    #[test]
    fn low_emissivity_pushes_readings_away_from_the_reflected_temperature() {
        let c = Correction::DEFAULT;
        // Hotter than the room reads hotter, colder reads colder, and the room
        // itself is unchanged.
        assert!(c.celsius(raw(80.0)) > 80.5);
        assert!(c.celsius(raw(0.0)) < -0.5);
        assert!((c.celsius(raw(20.0)) - 20.0).abs() < 0.05);
    }

    fn image() -> Image {
        // 0 1 2
        // 3 4 5
        Image {
            width: 3,
            height: 2,
            temps: (0..6).map(|i| i as f32).collect(),
        }
    }

    #[test]
    fn reports_extremes_and_points() {
        let s = image().stats();
        assert_eq!((s.min, s.max, s.mean), ((0.0, 0), (5.0, 5), 2.5));
        let report = image().report(Some((2, 1))).unwrap();
        assert!(report.contains("max\t5.00\n"));
        assert!(report.contains("point\t5.00\n"));
        assert!(image().report(Some((3, 0))).is_err());
    }
}
