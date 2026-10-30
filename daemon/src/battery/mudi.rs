/// Battery level from the GL MCU (`ubus call mcu status`), whose values come as strings or numbers.
use serde_json::Value;

use crate::{battery::BatteryState, error::RayhunterError};

pub async fn get_battery_state() -> Result<BatteryState, RayhunterError> {
    let out = tokio::process::Command::new("ubus")
        .args(["call", "mcu", "status"])
        .output()
        .await?;
    if !out.status.success() {
        return Err(RayhunterError::BatteryLevelParseError);
    }
    parse_mcu_status(&out.stdout)
}

fn field_as_u8(status: &Value, key: &str) -> Option<u8> {
    match status.get(key)? {
        Value::Number(n) => n.as_u64()?.try_into().ok(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_mcu_status(json: &[u8]) -> Result<BatteryState, RayhunterError> {
    let status: Value =
        serde_json::from_slice(json).map_err(|_| RayhunterError::BatteryLevelParseError)?;
    let level = field_as_u8(&status, "charge_percent")
        .filter(|l| *l <= 100)
        .ok_or(RayhunterError::BatteryLevelParseError)?;
    let is_plugged_in = field_as_u8(&status, "charging_status")
        .ok_or(RayhunterError::BatteryPluggedInStatusParseError)?
        != 0;
    Ok(BatteryState {
        level,
        is_plugged_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mudi_v2_strings() {
        let json = br#"{"temperature": "33.7", "charge_percent": "87", "charging_status": "0", "charge_cnt": "8"}"#;
        let state = parse_mcu_status(json).unwrap();
        assert_eq!(state.level, 87);
        assert!(!state.is_plugged_in);
    }

    #[test]
    fn parses_numbers() {
        let json = br#"{"temperature": "30.0", "charge_cnt": 1, "abnormal": false, "charging_status": 1, "charge_percent": 100}"#;
        let state = parse_mcu_status(json).unwrap();
        assert_eq!(state.level, 100);
        assert!(state.is_plugged_in);
    }

    #[test]
    fn rejects_missing_level() {
        assert!(parse_mcu_status(br#"{"charging_status": 1}"#).is_err());
    }
}
