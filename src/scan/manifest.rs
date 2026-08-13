use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize, Clone, Debug, Default)]
pub struct PackageJsonData {
    pub scripts: HashMap<String, String>,
    pub dependencies: Vec<String>,
    pub dev_dependencies: Vec<String>,
    pub main_entry: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct TsConfigData {
    pub strict: bool,
    pub target: Option<String>,
    pub path_aliases: HashMap<String, Vec<String>>,
}

fn object_keys(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

pub fn parse_package_json(text: &str) -> Option<(PackageJsonData, bool)> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;

    let declares_workspaces = json.get("workspaces").is_some();
    let dependencies = object_keys(json.get("dependencies"));
    let dev_dependencies = object_keys(json.get("devDependencies"));

    let scripts = json
        .get("scripts")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|val| (k.clone(), val.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let main_entry = json
        .get("main")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("module").and_then(|v| v.as_str()))
        .map(str::to_string);

    Some((
        PackageJsonData {
            scripts,
            dependencies,
            dev_dependencies,
            main_entry,
        },
        declares_workspaces,
    ))
}

pub fn parse_package_name(text: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    json.get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub fn parse_tsconfig_resolved(vfs: &crate::vfs::Vfs, path: &str) -> Option<TsConfigData> {
    const MAX_EXTENDS_DEPTH: usize = 8; // guards against a cyclic or pathological extends chain

    fn dirname(path: &str) -> &str {
        path.rfind('/').map(|i| &path[..i]).unwrap_or("")
    }

    fn join_relative(base_dir: &str, relative: &str) -> String {
        let mut segments: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
        for part in relative.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    segments.pop();
                }
                other => segments.push(other),
            }
        }
        segments.join("/")
    }

    fn resolve(vfs: &crate::vfs::Vfs, path: &str, depth: usize) -> Option<TsConfigData> {
        if depth >= MAX_EXTENDS_DEPTH {
            return None;
        }

        let text = vfs.read_utf8(path)?;
        let json: serde_json::Value = serde_json::from_str(text).ok()?;

        let base = json
            .get("extends")
            .and_then(|v| v.as_str())
            .and_then(|extends_value| {
                // Only relative paths can resolve — see doc comment above.
                if !(extends_value.starts_with("./") || extends_value.starts_with("../")) {
                    return None;
                }
                let mut resolved = join_relative(dirname(path), extends_value);
                if !resolved.ends_with(".json") {
                    resolved.push_str(".json");
                }
                resolve(vfs, &resolved, depth + 1)
            });

        let own = parse_tsconfig_fields(&json);

        Some(TsConfigData {
            strict: own
                .strict
                .or_else(|| base.as_ref().map(|b| b.strict))
                .unwrap_or(false),
            target: own
                .target
                .or_else(|| base.as_ref().and_then(|b| b.target.clone())),
            path_aliases: if own.path_aliases.is_empty() {
                base.map(|b| b.path_aliases).unwrap_or_default()
            } else {
                own.path_aliases
            },
        })
    }

    resolve(vfs, path, 0)
}

struct OwnTsConfigFields {
    strict: Option<bool>,
    target: Option<String>,
    path_aliases: HashMap<String, Vec<String>>,
}

fn parse_tsconfig_fields(json: &serde_json::Value) -> OwnTsConfigFields {
    let compiler_options = json
        .get("compilerOptions")
        .or_else(|| json.get("compiler_options"));

    let strict = compiler_options
        .and_then(|o| o.get("strict"))
        .and_then(|v| v.as_bool());

    let target = compiler_options
        .and_then(|o| o.get("target"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let path_aliases = compiler_options
        .and_then(|o| o.get("paths"))
        .and_then(|p| p.as_object())
        .map(|paths| {
            paths
                .iter()
                .filter_map(|(alias, targets)| {
                    let targets: Vec<String> = targets
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    (!targets.is_empty()).then_some((alias.clone(), targets))
                })
                .collect()
        })
        .unwrap_or_default();

    OwnTsConfigFields {
        strict,
        target,
        path_aliases,
    }
}

#[allow(dead_code)]
pub fn parse_tsconfig(text: &str) -> Option<TsConfigData> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let own = parse_tsconfig_fields(&json);
    Some(TsConfigData {
        strict: own.strict.unwrap_or(false),
        target: own.target,
        path_aliases: own.path_aliases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_json_dependencies_and_scripts() {
        let text = r#"{
            "name": "my-app",
            "scripts": { "build": "vite build" },
            "dependencies": { "react": "^18.0.0" },
            "devDependencies": { "vite": "^5.0.0" },
            "main": "dist/index.js"
        }"#;
        let (data, has_workspaces) = parse_package_json(text).unwrap();
        assert_eq!(data.dependencies, vec!["react".to_string()]);
        assert_eq!(data.dev_dependencies, vec!["vite".to_string()]);
        assert_eq!(data.main_entry.as_deref(), Some("dist/index.js"));
        assert!(!has_workspaces);
        assert_eq!(parse_package_name(text).as_deref(), Some("my-app"));
    }

    #[test]
    fn parses_tsconfig_path_aliases() {
        let text = r#"{
            "compilerOptions": {
                "strict": true,
                "target": "ES2022",
                "paths": { "@/*": ["src/*"] }
            }
        }"#;
        let data = parse_tsconfig(text).unwrap();
        assert!(data.strict);
        assert_eq!(data.target.as_deref(), Some("ES2022"));
        assert_eq!(
            data.path_aliases.get("@/*"),
            Some(&vec!["src/*".to_string()])
        );
    }

    #[test]
    fn malformed_json_yields_none_not_a_panic() {
        assert!(parse_package_json("{ not valid json").is_none());
        assert!(parse_tsconfig("also not valid").is_none());
    }

    #[test]
    fn resolves_relative_extends_chain_child_wins_on_conflict() {
        let mut vfs = crate::vfs::Vfs::empty();
        vfs.insert(
            "tsconfig.json",
            br#"{ "compilerOptions": { "strict": true, "target": "ES2020", "paths": { "@/*": ["src/*"] } } }"#
                .to_vec(),
        );
        vfs.insert(
            "tsconfig.build.json",
            br#"{ "extends": "./tsconfig.json", "compilerOptions": { "target": "ES2022" } }"#
                .to_vec(),
        );

        let data = parse_tsconfig_resolved(&vfs, "tsconfig.build.json").unwrap();
        // target: child overrides base
        assert_eq!(data.target.as_deref(), Some("ES2022"));
        // strict/paths: not specified by the child, inherited from base
        assert!(data.strict);
        assert_eq!(
            data.path_aliases.get("@/*"),
            Some(&vec!["src/*".to_string()])
        );
    }

    #[test]
    fn unresolvable_package_style_extends_falls_back_to_own_fields() {
        let mut vfs = crate::vfs::Vfs::empty();
        vfs.insert(
            "tsconfig.json",
            br#"{ "extends": "@tsconfig/node18/tsconfig.json", "compilerOptions": { "strict": true } }"#.to_vec(),
        );

        let data = parse_tsconfig_resolved(&vfs, "tsconfig.json").unwrap();
        assert!(data.strict);
    }

    #[test]
    fn nested_relative_extends_resolves_parent_directory_correctly() {
        let mut vfs = crate::vfs::Vfs::empty();
        vfs.insert(
            "tsconfig.base.json",
            br#"{ "compilerOptions": { "strict": true } }"#.to_vec(),
        );
        vfs.insert(
            "packages/app/tsconfig.json",
            br#"{ "extends": "../../tsconfig.base.json" }"#.to_vec(),
        );

        let data = parse_tsconfig_resolved(&vfs, "packages/app/tsconfig.json").unwrap();
        assert!(data.strict);
    }
}
