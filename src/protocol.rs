use serde::{Serialize, Deserialize};

/// i3bar click protocol input JSON object.
#[derive(Deserialize)]
pub struct Click {
    pub name: Option<String>,
    pub instance: Option<String>,
    pub button: u8,
    pub modifiers: Option<Vec<String>>,
    pub x: i16,
    pub y: i16,
    pub relative_x: i16,
    pub relative_y: i16,
    pub output_x: Option<i16>,
    pub output_y: Option<i16>,
    pub width: u16,
    pub height: u16,
}

/// i3bar protocol header JSON object.
/// The header is sent first and contains the version of the protocol.
#[derive(Serialize, Default)]
pub struct Header {
    pub version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_events: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cont_signal: Option<u8>,
}

/// i3bar protocol output JSON object.
#[derive(Serialize, Default)]
pub struct Output {
    /// Ex: `E: 10.0.0.1 (1000 Mbit/s)`
    pub full_text: String,
    /// Ex: `10.0.0.1`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_text: Option<String>,
    /// Ex: `#00ff00`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Ex: `#1c1c1c`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Ex: `#ee0000`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    /// Ex: `1`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_top: Option<i16>,
    /// Ex: `0`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_right: Option<i16>,
    /// Ex: `3`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_bottom: Option<i16>,
    /// Ex: `1`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_left: Option<i16>,
    /// Ex: `300`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_width: Option<u16>,
    /// Ex: `right`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<String>,
    /// Ex: `false`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urgent: Option<bool>,
    /// Ex: `ethernet`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Ex: `eth0`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Ex: `true`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub separator: Option<bool>,
    /// Ex: `9`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub separator_block_width: Option<u16>,
    /// Ex: `none`, `pango`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markup: Option<String>,
}
