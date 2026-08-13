use serde::Serialize;

#[derive(Serialize, Clone, Debug, Default)]
pub struct DockerComposeService {
    pub name: String,
    pub image: Option<String>,
    pub build_context: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct DockerComposeData {
    pub services: Vec<DockerComposeService>,
}

pub fn parse_docker_compose(text: &str) -> Option<DockerComposeData> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    let services_map = doc.get("services")?.as_mapping()?;

    let services = services_map
        .iter()
        .filter_map(|(name, definition)| {
            let name = name.as_str()?.to_string();
            let image = definition
                .get("image")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let build_context = definition.get("build").and_then(|build| {
                build
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| build.get("context")?.as_str().map(str::to_string))
            });
            Some(DockerComposeService {
                name,
                image,
                build_context,
            })
        })
        .collect();

    Some(DockerComposeData { services })
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct DockerfileData {
    pub base_image: Option<String>,
    pub exposed_ports: Vec<String>,
}

pub fn parse_dockerfile(text: &str) -> DockerfileData {
    let mut base_image = None;
    let mut exposed_ports = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("FROM ") {
            // `FROM node:20-alpine AS build` -> keep just the image ref,
            // discarding a possible `AS <stage>` suffix.
            base_image = rest.split_whitespace().next().map(str::to_string);
        } else if let Some(rest) = line.strip_prefix("EXPOSE ") {
            exposed_ports.extend(rest.split_whitespace().map(str::to_string));
        }
    }

    DockerfileData {
        base_image,
        exposed_ports,
    }
}

pub fn parse_pnpm_workspace_packages(text: &str) -> Option<Vec<String>> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    let packages = doc.get("packages")?.as_sequence()?;
    Some(
        packages
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_compose_services() {
        let text = "services:\n  web:\n    image: nginx:latest\n  api:\n    build:\n      context: ./api\n";
        let data = parse_docker_compose(text).unwrap();
        assert_eq!(data.services.len(), 2);
        assert_eq!(data.services[0].image.as_deref(), Some("nginx:latest"));
    }

    #[test]
    fn parses_dockerfile_from_and_expose() {
        let text = "FROM node:20-alpine AS build\nWORKDIR /app\nEXPOSE 3000 3001\n";
        let data = parse_dockerfile(text);
        assert_eq!(data.base_image.as_deref(), Some("node:20-alpine"));
        assert_eq!(
            data.exposed_ports,
            vec!["3000".to_string(), "3001".to_string()]
        );
    }

    #[test]
    fn parses_pnpm_workspace_globs() {
        let text = "packages:\n  - 'packages/*'\n  - 'apps/*'\n";
        let globs = parse_pnpm_workspace_packages(text).unwrap();
        assert_eq!(globs, vec!["packages/*".to_string(), "apps/*".to_string()]);
    }

    #[test]
    fn malformed_yaml_yields_none_not_a_panic() {
        assert!(parse_docker_compose("not: [valid, yaml:").is_none());
    }
}
