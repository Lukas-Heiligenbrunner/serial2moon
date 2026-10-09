//! Optimistic motion tracking from the G-code we send to the printer.
//!
//! Marlin doesn't continuously report position, so (like OctoPrint/Klipper) we track
//! the commanded toolhead position from outgoing `G0/G1` moves, honoring absolute/relative
//! mode (`G90/G91`, `M82/M83`), `G92` set-position, and `G28` homing. This drives the
//! position and `homed_axes` shown in the frontend.

#[derive(Debug, Clone)]
pub struct Motion {
    absolute: bool,
    abs_extrude: bool,
    pos: [f64; 4], // X, Y, Z, E
    homed: [bool; 3],
    /// Net filament moved by the extruder (mm): retracts subtract, `G92 E` re-zeroing
    /// doesn't count. Differences of this feed Klipper's `print_stats.filament_used`.
    extruded: f64,
}

impl Default for Motion {
    fn default() -> Self {
        Motion {
            absolute: true,
            abs_extrude: true,
            pos: [0.0; 4],
            homed: [false; 3],
            extruded: 0.0,
        }
    }
}

impl Motion {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn position(&self) -> [f64; 4] {
        self.pos
    }

    pub fn extruded(&self) -> f64 {
        self.extruded
    }

    pub fn absolute(&self) -> bool {
        self.absolute
    }

    pub fn absolute_extrude(&self) -> bool {
        self.abs_extrude
    }

    /// Homed axes as Klipper expects them, e.g. `"xyz"` or `""`.
    pub fn homed_axes(&self) -> String {
        ['x', 'y', 'z']
            .iter()
            .enumerate()
            .filter(|(i, _)| self.homed[*i])
            .map(|(_, c)| *c)
            .collect()
    }

    /// Apply one outgoing G-code line. Returns true if motion-relevant state changed.
    pub fn apply(&mut self, line: &str) -> bool {
        let up = line.trim().to_ascii_uppercase();
        let word = up.split_whitespace().next().unwrap_or("");
        match word {
            "G90" => {
                self.absolute = true;
                self.abs_extrude = true;
                true
            }
            "G91" => {
                self.absolute = false;
                self.abs_extrude = false;
                true
            }
            "M82" => {
                self.abs_extrude = true;
                true
            }
            "M83" => {
                self.abs_extrude = false;
                true
            }
            "G92" => {
                let mut changed = false;
                for (i, axis) in ['X', 'Y', 'Z', 'E'].into_iter().enumerate() {
                    if let Some(v) = axis_value(&up, axis) {
                        self.pos[i] = v;
                        changed = true;
                    }
                }
                changed
            }
            "G28" => {
                // No axis words => home all; otherwise only the listed axes.
                let mut listed = [false; 3];
                let mut any = false;
                for (i, axis) in ['X', 'Y', 'Z'].into_iter().enumerate() {
                    if up.split_whitespace().skip(1).any(|t| t.starts_with(axis)) {
                        listed[i] = true;
                        any = true;
                    }
                }
                for (i, &is_listed) in listed.iter().enumerate() {
                    if !any || is_listed {
                        self.homed[i] = true;
                        self.pos[i] = 0.0;
                    }
                }
                true
            }
            "G0" | "G1" => {
                let mut changed = false;
                for (i, axis) in ['X', 'Y', 'Z'].into_iter().enumerate() {
                    if let Some(v) = axis_value(&up, axis) {
                        self.pos[i] = if self.absolute { v } else { self.pos[i] + v };
                        changed = true;
                    }
                }
                if let Some(v) = axis_value(&up, 'E') {
                    let e = if self.abs_extrude { v } else { self.pos[3] + v };
                    self.extruded += e - self.pos[3];
                    self.pos[3] = e;
                    changed = true;
                }
                changed
            }
            _ => false,
        }
    }
}

fn axis_value(line: &str, axis: char) -> Option<f64> {
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix(axis).and_then(|v| v.parse::<f64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_absolute_moves_and_homing() {
        let mut m = Motion::new();
        assert_eq!(m.homed_axes(), "");
        assert!(m.apply("G28"));
        assert_eq!(m.homed_axes(), "xyz");
        assert!(m.apply("G1 X10 Y20 Z0.3 E1.5 F1500"));
        assert_eq!(m.position(), [10.0, 20.0, 0.3, 1.5]);
    }

    #[test]
    fn honors_relative_mode() {
        let mut m = Motion::new();
        m.apply("G1 X10 Y10");
        m.apply("G91");
        assert!(!m.absolute());
        m.apply("G1 X5 Y-3");
        assert_eq!(&m.position()[..2], &[15.0, 7.0]);
    }

    #[test]
    fn g28_partial_homes_only_listed_axes() {
        let mut m = Motion::new();
        m.apply("G28 X Y");
        assert_eq!(m.homed_axes(), "xy");
    }

    #[test]
    fn g92_sets_position() {
        let mut m = Motion::new();
        m.apply("G92 E0");
        assert_eq!(m.position()[3], 0.0);
        m.apply("G1 E5");
        assert_eq!(m.position()[3], 5.0);
    }

    #[test]
    fn tracks_net_extrusion_in_both_e_modes() {
        let mut m = Motion::new();
        m.apply("M83"); // relative E (PrusaSlicer's default)
        m.apply("G1 X5 E1.0");
        m.apply("G1 E-0.8"); // retract
        m.apply("G1 E0.8"); // unretract
        assert!((m.extruded() - 1.0).abs() < 1e-9, "{}", m.extruded());
        m.apply("M82"); // absolute E
        m.apply("G92 E0"); // re-zeroing the E counter is not a movement
        m.apply("G1 X10 E2.5");
        m.apply("G1 X20 E4.0");
        assert!((m.extruded() - 5.0).abs() < 1e-9, "{}", m.extruded());
    }

    #[test]
    fn ignores_non_motion() {
        let mut m = Motion::new();
        assert!(!m.apply("M104 S200"));
        assert!(!m.apply("M117 hello"));
    }
}
