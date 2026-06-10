//! Classification of inbound Marlin lines.

/// A temperature report parsed from `T:.. /.. B:.. /..`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TempReport {
    pub ext_temp: Option<f64>,
    pub ext_target: Option<f64>,
    pub ext_power: Option<f64>,
    pub bed_temp: Option<f64>,
    pub bed_target: Option<f64>,
    pub bed_power: Option<f64>,
}

impl TempReport {
    pub fn is_empty(&self) -> bool {
        self.ext_temp.is_none() && self.bed_temp.is_none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// Acknowledgement. May carry inline temperature data (M105-style `ok T:..`).
    Ok(TempReport),
    /// Unsolicited temperature autoreport (M155).
    Temp(TempReport),
    /// `busy: processing` — printer alive but working; resets the ack timeout.
    Busy,
    /// `Resend: N` — printer wants a line resent.
    Resend(u64),
    /// `echo:` informational message.
    Echo(String),
    /// `Error:` / `!!` error message.
    Error(String),
    /// `start` power-up / reset banner.
    Start,
    /// Anything else (forwarded to the console).
    Other(String),
}

/// Parse a temperature report out of a line. Handles `T:`, `T0:`, `B:` with optional
/// `/target` and `@:`/`B@:` power tokens. Returns an empty report if nothing matched.
pub fn parse_temps(line: &str) -> TempReport {
    let mut r = TempReport::default();
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        // A sensor token looks like `T:25.00` (optionally followed by `/0.00`).
        if let Some((label, value)) = tok.split_once(':') {
            let value = value.parse::<f64>().ok();
            // optional `/target` in the next token
            let target = tokens
                .get(i + 1)
                .and_then(|t| t.strip_prefix('/'))
                .and_then(|t| t.parse::<f64>().ok());
            match label {
                "T" | "T0" => {
                    r.ext_temp = value;
                    if target.is_some() {
                        r.ext_target = target;
                        i += 1;
                    }
                }
                "B" => {
                    r.bed_temp = value;
                    if target.is_some() {
                        r.bed_target = target;
                        i += 1;
                    }
                }
                "@" => r.ext_power = value.map(|v| v / 127.0),
                "B@" => r.bed_power = value.map(|v| v / 127.0),
                _ => {}
            }
        }
        i += 1;
    }
    r
}

/// Classify a single line of printer output.
pub fn classify(raw: &str) -> Line {
    let line = raw.trim();
    let lower = line.to_ascii_lowercase();

    if lower == "start" {
        return Line::Start;
    }
    if lower.starts_with("busy:") {
        return Line::Busy;
    }
    if let Some(rest) = line
        .strip_prefix("Resend:")
        .or_else(|| line.strip_prefix("rs "))
        && let Ok(n) = rest.trim().parse::<u64>()
    {
        return Line::Resend(n);
    }
    if lower.starts_with("ok") {
        // `ok` may carry inline temps (response to M105).
        let temps = parse_temps(line);
        return Line::Ok(temps);
    }
    if line.starts_with("!!") || lower.starts_with("error") {
        return Line::Error(line.trim_start_matches('!').trim().to_string());
    }
    if let Some(rest) = line.strip_prefix("echo:") {
        return Line::Echo(rest.trim().to_string());
    }
    // A bare temperature autoreport line (`T:.. B:..`).
    let temps = parse_temps(line);
    if !temps.is_empty() {
        return Line::Temp(temps);
    }
    Line::Other(line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inline_ok_temps() {
        let t = match classify("ok T:204.71 /205.00 B:59.90 /60.00 @:80 B@:40") {
            Line::Ok(t) => t,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(t.ext_temp, Some(204.71));
        assert_eq!(t.ext_target, Some(205.0));
        assert_eq!(t.bed_temp, Some(59.90));
        assert_eq!(t.bed_target, Some(60.0));
        assert_eq!(t.ext_power, Some(80.0 / 127.0));
    }

    #[test]
    fn classifies_bare_autoreport_as_temp() {
        assert!(matches!(classify("T:25.0 /0.0 B:24.0 /0.0"), Line::Temp(_)));
    }

    #[test]
    fn classifies_control_lines() {
        assert!(matches!(classify("busy: processing"), Line::Busy));
        assert!(matches!(classify("Resend: 42"), Line::Resend(42)));
        assert!(matches!(classify("start"), Line::Start));
        assert!(matches!(classify("echo:busy"), Line::Echo(_)));
        assert!(matches!(
            classify("Error:checksum mismatch"),
            Line::Error(_)
        ));
        assert!(matches!(classify("ok"), Line::Ok(_)));
    }
}
