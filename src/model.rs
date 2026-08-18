use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub layouts: Vec<MonitorLayout>,
    /// Last healthy topology, kept separately from user layouts.
    #[serde(default)]
    pub last_known_working: Option<MonitorLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorLayout {
    pub name: String,
    #[serde(default)]
    pub monitors: Vec<MonitorConfig>,
    #[serde(default)]
    pub hotkey: Option<HotkeyBinding>,
    #[serde(default)]
    pub controller_hotkey: Option<ControllerBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerBinding {
    pub vendor_id: u16,
    pub product_id: u16,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub buttons: Vec<u32>,
    #[serde(default)]
    pub button_labels: Vec<String>,
}

impl ControllerBinding {
    pub fn button_label(&self) -> String {
        if self.button_labels.len() == self.buttons.len() && !self.button_labels.is_empty() {
            self.button_labels.join(" + ")
        } else {
            self.buttons
                .iter()
                .map(|button| format!("Button {}", button + 1))
                .collect::<Vec<_>>()
                .join(" + ")
        }
    }

    pub fn label(&self) -> String {
        let device = if self.device_name.is_empty() {
            format!("Controller {:04X}:{:04X}", self.vendor_id, self.product_id)
        } else {
            self.device_name.clone()
        };
        format!("{device}: {}", self.button_label())
    }

    pub fn is_valid(&self) -> bool {
        !self.buttons.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    pub identity: MonitorIdentity,
    pub friendly_name: String,
    pub enabled: bool,
    pub source_adapter_low: u32,
    pub source_adapter_high: i32,
    pub source_id: u32,
    #[serde(default)]
    pub clone_group: Option<u32>,
    /// Raw `DISPLAYCONFIG_ROTATION`; `None` preserves the current value.
    #[serde(default)]
    pub rotation: Option<i32>,
    /// Source-to-target `DISPLAYCONFIG_SCALING`, not DPI scaling.
    #[serde(default)]
    pub scaling: Option<i32>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    /// Exact captured refresh rate, retained while it matches `refresh_hz`.
    #[serde(default)]
    pub refresh_numerator: Option<u32>,
    #[serde(default)]
    pub refresh_denominator: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct MonitorIdentity {
    pub device_path: String,
    pub adapter_low: u32,
    pub adapter_high: i32,
    pub target_id: u32,
}

impl MonitorConfig {
    pub fn source_key(&self) -> (i32, u32, u32) {
        (
            self.source_adapter_high,
            self.source_adapter_low,
            self.source_id,
        )
    }
}

impl MonitorIdentity {
    pub fn stable_key(&self) -> String {
        if !self.device_path.is_empty() {
            self.device_path.to_ascii_lowercase()
        } else {
            format!(
                "adapter:{}:{}:target:{}",
                self.adapter_high, self.adapter_low, self.target_id
            )
        }
    }

    pub fn matches(&self, other: &Self) -> bool {
        if !self.device_path.is_empty() && !other.device_path.is_empty() {
            self.device_path.eq_ignore_ascii_case(&other.device_path)
        } else {
            self.adapter_low == other.adapter_low
                && self.adapter_high == other.adapter_high
                && self.target_id == other.target_id
        }
    }

    pub fn display_label(&self) -> String {
        if self.device_path.is_empty() {
            return format!(
                "adapter {}:{} / target {}",
                self.adapter_high, self.adapter_low, self.target_id
            );
        }

        // The trailing interface GUID is shared; hardware and instance are unique.
        let mut parts = self.device_path.split('#');
        let _kind = parts.next();
        match (parts.next(), parts.next()) {
            (Some(hardware), Some(instance)) => format!("{hardware} / {instance}"),
            _ => self.device_path.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key: HotkeyKey,
}

impl Default for HotkeyBinding {
    fn default() -> Self {
        Self {
            ctrl: true,
            alt: true,
            shift: false,
            win: false,
            key: HotkeyKey::F1,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HotkeyKey {
    F1 = 0,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
}

impl HotkeyKey {
    pub const ALL: [Self; 22] = [
        Self::F1,
        Self::F2,
        Self::F3,
        Self::F4,
        Self::F5,
        Self::F6,
        Self::F7,
        Self::F8,
        Self::F9,
        Self::F10,
        Self::F11,
        Self::F12,
        Self::Num0,
        Self::Num1,
        Self::Num2,
        Self::Num3,
        Self::Num4,
        Self::Num5,
        Self::Num6,
        Self::Num7,
        Self::Num8,
        Self::Num9,
    ];

    pub fn label(self) -> &'static str {
        const LABELS: [&str; 22] = [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "0", "1",
            "2", "3", "4", "5", "6", "7", "8", "9",
        ];
        LABELS[self as usize]
    }

    pub fn vk(self) -> u32 {
        let index = self as u32;
        if index < 12 {
            0x70 + index
        } else {
            0x30 + index - 12
        }
    }
}

impl std::fmt::Display for HotkeyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl HotkeyBinding {
    pub fn has_modifier(self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }

    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.win {
            parts.push("Win");
        }
        parts.push(self.key.label());
        parts.join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(device_path: &str) -> MonitorIdentity {
        MonitorIdentity {
            device_path: device_path.to_string(),
            adapter_low: 1,
            adapter_high: 0,
            target_id: 1,
        }
    }

    #[test]
    fn identity_label_uses_unique_device_path_components() {
        let monitor =
            identity(r"\\?DISPLAY#DEL1234#5&AAA111&0&UID1#{E6F07B5F-EE97-4A90-B076-33F57BF4EAA7}");

        assert_eq!(monitor.display_label(), "DEL1234 / 5&AAA111&0&UID1");
    }

    #[test]
    fn shared_interface_guid_does_not_collapse_monitor_identity() {
        let first =
            identity(r"\\?DISPLAY#DEL1234#5&AAA111&0&UID1#{E6F07B5F-EE97-4A90-B076-33F57BF4EAA7}");
        let second =
            identity(r"\\?DISPLAY#DEL1234#5&BBB222&0&UID2#{E6F07B5F-EE97-4A90-B076-33F57BF4EAA7}");

        assert_ne!(first.stable_key(), second.stable_key());
        assert!(!first.matches(&second));
    }

    #[test]
    fn older_config_files_default_to_no_recovery_snapshot() {
        let config: AppConfig =
            serde_json::from_str(r#"{"layouts":[]}"#).expect("legacy config should load");

        assert!(config.last_known_working.is_none());
    }

    #[test]
    fn hotkey_labels_and_virtual_keys_keep_their_windows_ranges() {
        assert_eq!((HotkeyKey::F1.label(), HotkeyKey::F1.vk()), ("F1", 0x70));
        assert_eq!((HotkeyKey::F12.label(), HotkeyKey::F12.vk()), ("F12", 0x7b));
        assert_eq!((HotkeyKey::Num0.label(), HotkeyKey::Num0.vk()), ("0", 0x30));
        assert_eq!((HotkeyKey::Num9.label(), HotkeyKey::Num9.vk()), ("9", 0x39));
    }
}
