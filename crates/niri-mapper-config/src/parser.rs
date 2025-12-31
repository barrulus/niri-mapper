//! KDL configuration parser

use std::path::Path;
use crate::error::ConfigError;
use crate::model::*;

/// Parse a configuration file from the given path
pub fn parse_config(path: &Path) -> Result<Config, ConfigError> {
    let content = std::fs::read_to_string(path)?;
    parse_config_str(&content)
}

/// Parse configuration from a string
pub fn parse_config_str(content: &str) -> Result<Config, ConfigError> {
    let doc: kdl::KdlDocument = content.parse().map_err(|e: kdl::KdlError| {
        ConfigError::ParseError {
            src: content.to_string(),
            span: (0, content.len()).into(),
            source: e,
        }
    })?;

    let mut config = Config::default();

    for node in doc.nodes() {
        match node.name().value() {
            "global" => {
                config.global = parse_global(node)?;
            }
            "device" => {
                config.devices.push(parse_device(node)?);
            }
            name => {
                tracing::warn!("Unknown top-level node: {}", name);
            }
        }
    }

    Ok(config)
}

fn parse_global(node: &kdl::KdlNode) -> Result<GlobalConfig, ConfigError> {
    let mut global = GlobalConfig::default();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "log-level" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_string() {
                            global.log_level = val.parse().map_err(|e| ConfigError::Invalid {
                                message: e,
                            })?;
                        }
                    }
                }
                "niri-keybinds-path" => {
                    if let Some(entry) = child.entries().first() {
                        if let Some(val) = entry.value().as_string() {
                            global.niri_keybinds_path = shellexpand::tilde(val).into_owned().into();
                        }
                    }
                }
                name => {
                    tracing::warn!("Unknown global config option: {}", name);
                }
            }
        }
    }

    Ok(global)
}

fn parse_device(node: &kdl::KdlNode) -> Result<DeviceConfig, ConfigError> {
    let name = node
        .entries()
        .first()
        .and_then(|e| e.value().as_string())
        .map(|s| s.to_string());

    let mut device = DeviceConfig {
        name,
        vendor_product: None,
        profiles: std::collections::HashMap::new(),
    };

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "vendor-product" => {
                    if let Some(entry) = child.entries().first() {
                        device.vendor_product = entry.value().as_string().map(|s| s.to_string());
                    }
                }
                "profile" => {
                    let profile_name = child
                        .entries()
                        .first()
                        .and_then(|e| e.value().as_string())
                        .unwrap_or("default")
                        .to_string();
                    let profile = parse_profile(child)?;
                    device.profiles.insert(profile_name, profile);
                }
                name => {
                    tracing::warn!("Unknown device config option: {}", name);
                }
            }
        }
    }

    Ok(device)
}

fn parse_profile(node: &kdl::KdlNode) -> Result<Profile, ConfigError> {
    let mut profile = Profile::default();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "remap" => {
                    profile.remap = parse_key_value_block(child)?;
                }
                "combo" => {
                    profile.combo = parse_key_value_block(child)?;
                }
                "macro" => {
                    profile.macros = parse_macro_block(child)?;
                }
                "niri-passthrough" => {
                    profile.niri_passthrough = parse_niri_passthrough(child)?;
                }
                name => {
                    tracing::warn!("Unknown profile option: {}", name);
                }
            }
        }
    }

    Ok(profile)
}

fn parse_key_value_block(
    node: &kdl::KdlNode,
) -> Result<std::collections::HashMap<String, String>, ConfigError> {
    let mut map = std::collections::HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value().to_string();
            if let Some(entry) = child.entries().first() {
                if let Some(val) = entry.value().as_string() {
                    map.insert(key, val.to_string());
                }
            }
        }
    }

    Ok(map)
}

fn parse_macro_block(
    node: &kdl::KdlNode,
) -> Result<std::collections::HashMap<String, Vec<MacroAction>>, ConfigError> {
    let mut map = std::collections::HashMap::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value().to_string();
            let mut actions = Vec::new();

            for entry in child.entries() {
                if let Some(val) = entry.value().as_string() {
                    if val.starts_with("delay(") && val.ends_with(')') {
                        // Parse delay(ms)
                        let ms_str = &val[6..val.len() - 1];
                        if let Ok(ms) = ms_str.parse::<u64>() {
                            actions.push(MacroAction::Delay(ms));
                        }
                    } else {
                        actions.push(MacroAction::Key(val.to_string()));
                    }
                }
            }

            map.insert(key, actions);
        }
    }

    Ok(map)
}

fn parse_niri_passthrough(node: &kdl::KdlNode) -> Result<Vec<NiriKeybind>, ConfigError> {
    let mut keybinds = Vec::new();

    if let Some(children) = node.children() {
        for child in children.nodes() {
            let key = child.name().value().to_string();

            // The action is in the child's children block
            if let Some(action_children) = child.children() {
                // Reconstruct the action from the children
                let action = action_children
                    .nodes()
                    .iter()
                    .map(|n| {
                        let mut s = n.name().value().to_string();
                        for entry in n.entries() {
                            if let Some(v) = entry.value().as_string() {
                                s.push_str(&format!(" \"{}\"", v));
                            } else if let Some(v) = entry.value().as_i64() {
                                s.push_str(&format!(" {}", v));
                            }
                        }
                        s
                    })
                    .collect::<Vec<_>>()
                    .join("; ");

                keybinds.push(NiriKeybind {
                    key,
                    action: format!("{};", action),
                });
            }
        }
    }

    Ok(keybinds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_config() {
        let config = r#"
            global {
                log-level "debug"
            }

            device "Test Keyboard" {
                profile "default" {
                    remap {
                        CapsLock "Escape"
                    }
                }
            }
        "#;

        let result = parse_config_str(config).unwrap();
        assert_eq!(result.global.log_level, LogLevel::Debug);
        assert_eq!(result.devices.len(), 1);
        assert_eq!(result.devices[0].name, Some("Test Keyboard".to_string()));
    }
}
