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
    /// `busy: processing` (Prusa: `echo:busy: processing` / `echo:busy: paused for user`)
    /// — printer alive but working; resets the ack timeout.
    Busy,
    /// `Resend: N` — printer wants a line resent.
    Resend(u64),
    /// `RX timeout` (Prusa) — the printer received part of a line but no newline within
    /// 2 s and discarded it. It sends no `ok` and no `Resend`, so the line must be re-sent.
    RxTimeout,
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
    if lower.starts_with("busy:") || lower.starts_with("echo:busy:") {
        return Line::Busy;
    }
    if lower == "rx timeout" {
        return Line::RxTimeout;
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

/// Parse an `M115` reply into `(firmware_name, machine_type)`, e.g.
/// `FIRMWARE_NAME:Prusa-Firmware 3.13.2 based on Marlin ... MACHINE_TYPE:Prusa i3 MK3S ...`.
/// Returns None if the line isn't an M115 reply. `machine_type` is empty if not present.
pub fn parse_firmware(line: &str) -> Option<(String, String)> {
    if !line.contains("FIRMWARE_NAME:") {
        return None;
    }
    // M115 is `KEY:value KEY:value ...` where values may contain spaces. Walk tokens,
    // starting a new field on an uppercase `KEY:` token and appending the rest.
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut key: Option<String> = None;
    let mut val = String::new();
    let flush = |key: &mut Option<String>, val: &mut String, pairs: &mut Vec<_>| {
        if let Some(k) = key.take() {
            pairs.push((k, val.trim().to_string()));
            val.clear();
        }
    };
    for tok in line.split_whitespace() {
        match tok.split_once(':') {
            Some((k, v))
                if !k.is_empty() && k.chars().all(|c| c.is_ascii_uppercase() || c == '_') =>
            {
                flush(&mut key, &mut val, &mut pairs);
                key = Some(k.to_string());
                val.push_str(v);
            }
            _ => {
                if !val.is_empty() {
                    val.push(' ');
                }
                val.push_str(tok);
            }
        }
    }
    flush(&mut key, &mut val, &mut pairs);

    let find = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    find("FIRMWARE_NAME").map(|fw| (fw, find("MACHINE_TYPE").unwrap_or_default()))
}

/// Parse a Marlin/Prusa host *action command* (sent when the printer's LCD is used during
/// a USB print), e.g. `//action:pause`, `// action:cancel`. Returns the lower-cased action
/// (`pause`/`resume`/`cancel`/…) or None.
pub fn parse_action(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("//")?.trim_start();
    rest.strip_prefix("action:")
        .map(|cmd| cmd.trim().to_ascii_lowercase())
}

/// Parse a Prusa `M850` sheet report into `(id, label, z_offset, active)`, e.g.
/// `Sheet 0 Z-1.3150 R-526 LSmooth1 B60 P0 A0` -> `(0, "Smooth1", -1.315, false)`.
/// Returns None for non-sheet lines or sheets without a usable label+offset (uncalibrated).
pub fn parse_sheet(line: &str) -> Option<(u8, String, f64, bool)> {
    let rest = line.trim().strip_prefix("Sheet ")?;
    let mut tokens = rest.split_whitespace();
    let id: u8 = tokens.next()?.parse().ok()?;
    let mut z = None;
    let mut label = None;
    let mut active = false;
    for tok in tokens {
        if let Some(v) = tok.strip_prefix('Z') {
            z = v.parse::<f64>().ok();
        } else if let Some(v) = tok.strip_prefix('A') {
            active = v == "1";
        } else if let Some(v) = tok.strip_prefix('L')
            && !v.is_empty()
        {
            label = Some(v.to_string());
        }
    }
    Some((id, label?, z?, active))
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

    #[test]
    fn classifies_prusa_echo_busy_as_busy() {
        // Prusa's host keepalive carries an `echo:` prefix.
        assert!(matches!(classify("echo:busy: processing"), Line::Busy));
        assert!(matches!(classify("echo:busy: paused for user"), Line::Busy));
    }

    #[test]
    fn classifies_prusa_rx_timeout() {
        // Prusa drops a partial line after 2 s without its newline — no ok, no Resend.
        assert!(matches!(classify("RX timeout"), Line::RxTimeout));
    }

    #[test]
    fn parses_m115_firmware_and_machine() {
        let line = "FIRMWARE_NAME:Prusa-Firmware 3.13.2 based on Marlin \
                    FIRMWARE_URL:https://github.com/prusa3d PROTOCOL_VERSION:1.0 \
                    MACHINE_TYPE:Prusa i3 MK3S EXTRUDER_COUNT:1 UUID:abc";
        let (fw, machine) = parse_firmware(line).expect("should parse");
        assert_eq!(fw, "Prusa-Firmware 3.13.2 based on Marlin");
        assert_eq!(machine, "Prusa i3 MK3S");
        assert!(parse_firmware("ok").is_none());
    }

    #[test]
    fn parses_m850_sheet_report() {
        let (id, label, z, active) =
            parse_sheet("Sheet 0 Z-1.3150 R-526 LSmooth1 B60 P0 A0").unwrap();
        assert_eq!(id, 0);
        assert_eq!(label, "Smooth1");
        assert!((z - -1.315).abs() < 1e-6);
        assert!(!active);
        let (id, label, _, active) =
            parse_sheet("Sheet 2 Z-1.5500 R-620 LTextur1 B0 P0 A1").unwrap();
        assert_eq!((id, label.as_str(), active), (2, "Textur1", true));
        assert!(parse_sheet("ok").is_none());
        assert!(parse_sheet("Sheet 3 uncalibrated").is_none());
    }

    #[test]
    fn parses_host_action_commands() {
        assert_eq!(parse_action("//action:pause").as_deref(), Some("pause"));
        assert_eq!(parse_action("// action:cancel").as_deref(), Some("cancel"));
        assert_eq!(parse_action("//action:resume").as_deref(), Some("resume"));
        assert_eq!(parse_action("echo:busy"), None);
        assert_eq!(parse_action("// regular comment"), None);
    }
}
