//! RezStudio's own settings, kept apart from Zed's so merges from Zed touch
//! as little as possible.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Whether a pane darkens what R draws to suit a dark theme. Only what's
/// shown is darkened: copies, exports and files keep R's own colors.
#[derive(
    Debug,
    PartialEq,
    Eq,
    Clone,
    Copy,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum DarkModeContent {
    #[default]
    Off,
    On,
    /// On while the app's theme is dark.
    FollowTheme,
}

impl DarkModeContent {
    pub fn is_on(self, theme_is_dark: bool) -> bool {
        match self {
            DarkModeContent::Off => false,
            DarkModeContent::On => true,
            DarkModeContent::FollowTheme => theme_is_dark,
        }
    }
}

#[with_fallible_options]
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, PartialEq)]
pub struct PlotsSettingsContent {
    /// Whether the Plots pane darkens plots to suit a dark theme: "off", "on",
    /// or "follow_theme". Copied and saved plots keep R's colors.
    ///
    /// Default: off
    pub dark_mode: Option<DarkModeContent>,
}

#[with_fallible_options]
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, PartialEq)]
pub struct ViewerSettingsContent {
    /// Whether the Viewer pane darkens pages to suit a dark theme: "off", "on",
    /// or "follow_theme". Pages opened in a browser keep their own colors.
    ///
    /// Default: off
    pub dark_mode: Option<DarkModeContent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dark_mode() {
        let content: PlotsSettingsContent =
            serde_json::from_str(r#"{ "dark_mode": "follow_theme" }"#).unwrap();
        assert_eq!(content.dark_mode, Some(DarkModeContent::FollowTheme));
        assert!(DarkModeContent::FollowTheme.is_on(true));
        assert!(!DarkModeContent::FollowTheme.is_on(false));
        assert!(DarkModeContent::On.is_on(false));
        assert!(!DarkModeContent::Off.is_on(true));
    }
}
