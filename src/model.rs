use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub layouts: Vec<MonitorLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorLayout {
    pub name: String,
    #[serde(default)]
    pub monitors: Vec<MonitorConfig>,
    #[serde(default)]
    pub hotkey: Option<HotkeyBinding>,
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
    /// Raw DISPLAYCONFIG_ROTATION value captured from the target path.
    /// None keeps the currently available path value for older config files.
    #[serde(default)]
    pub rotation: Option<i32>,
    /// Raw DISPLAYCONFIG_SCALING value captured from the target path.
    /// This is source-to-target scaling, not Windows per-monitor DPI scaling.
    #[serde(default)]
    pub scaling: Option<i32>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    /// Exact rational captured from DISPLAYCONFIG_PATH_TARGET_INFO. Used while
    /// refresh_hz still represents the same value, avoiding 59.94-style drift.
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

        // A monitor interface path normally ends in the same Windows display
        // interface class GUID for every monitor. Show the hardware and instance
        // components instead; those are the useful distinguishing portions.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HotkeyKey {
    F1,
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
        match self {
            Self::F1 => "F1",
            Self::F2 => "F2",
            Self::F3 => "F3",
            Self::F4 => "F4",
            Self::F5 => "F5",
            Self::F6 => "F6",
            Self::F7 => "F7",
            Self::F8 => "F8",
            Self::F9 => "F9",
            Self::F10 => "F10",
            Self::F11 => "F11",
            Self::F12 => "F12",
            Self::Num0 => "0",
            Self::Num1 => "1",
            Self::Num2 => "2",
            Self::Num3 => "3",
            Self::Num4 => "4",
            Self::Num5 => "5",
            Self::Num6 => "6",
            Self::Num7 => "7",
            Self::Num8 => "8",
            Self::Num9 => "9",
        }
    }

    pub fn vk(self) -> u32 {
        match self {
            Self::F1 => 0x70,
            Self::F2 => 0x71,
            Self::F3 => 0x72,
            Self::F4 => 0x73,
            Self::F5 => 0x74,
            Self::F6 => 0x75,
            Self::F7 => 0x76,
            Self::F8 => 0x77,
            Self::F9 => 0x78,
            Self::F10 => 0x79,
            Self::F11 => 0x7A,
            Self::F12 => 0x7B,
            Self::Num0 => 0x30,
            Self::Num1 => 0x31,
            Self::Num2 => 0x32,
            Self::Num3 => 0x33,
            Self::Num4 => 0x34,
            Self::Num5 => 0x35,
            Self::Num6 => 0x36,
            Self::Num7 => 0x37,
            Self::Num8 => 0x38,
            Self::Num9 => 0x39,
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
}
