use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Locale {
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh_CN")]
    ZhCn,
}

impl Default for Locale {
    fn default() -> Self {
        Locale::En
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Locale::En => write!(f, "en"),
            Locale::ZhCn => write!(f, "zh_CN"),
        }
    }
}

impl FromStr for Locale {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "en" | "english" => Ok(Locale::En),
            "zh" | "zh_cn" | "zh-cn" | "chinese" | "简体中文"
            | "zh_tw" | "zh-tw" | "zh_hk" | "zh-hk" | "繁體中文" => Ok(Locale::ZhCn),
            other => Err(format!("unsupported locale: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_round_trips_through_from_str() {
        assert_eq!(Locale::En.to_string().parse::<Locale>().unwrap(), Locale::En);
        assert_eq!(Locale::ZhCn.to_string().parse::<Locale>().unwrap(), Locale::ZhCn);
    }

    #[test]
    fn from_str_accepts_common_aliases() {
        assert_eq!("en".parse::<Locale>().unwrap(), Locale::En);
        assert_eq!("English".parse::<Locale>().unwrap(), Locale::En);
        assert_eq!("zh".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("zh_CN".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("zh-cn".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("简体中文".parse::<Locale>().unwrap(), Locale::ZhCn);
        // zh_TW / zh_HK fall back to ZhCn (no separate Traditional variant yet)
        assert_eq!("zh_TW".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("zh-tw".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("zh_HK".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("zh-hk".parse::<Locale>().unwrap(), Locale::ZhCn);
        assert_eq!("繁體中文".parse::<Locale>().unwrap(), Locale::ZhCn);
    }

    #[test]
    fn from_str_rejects_unknown() {
        assert!("fr".parse::<Locale>().is_err());
        assert!("".parse::<Locale>().is_err());
    }

    #[test]
    fn serde_uses_short_keys() {
        let s = serde_json::to_string(&Locale::ZhCn).unwrap();
        assert_eq!(s, r#""zh_CN""#);
        let parsed: Locale = serde_json::from_str(r#""en""#).unwrap();
        assert_eq!(parsed, Locale::En);
    }
}
