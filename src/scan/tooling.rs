pub fn is_root_file(path: &str, active_root: &str, filename: &str) -> bool {
    if active_root.is_empty() {
        path == filename
    } else {
        path.starts_with(active_root) && &path[active_root.len()..] == filename
    }
}

pub struct LockfileMatch {
    pub package_manager: &'static str,
}

pub fn match_lockfile(path: &str, active_root: &str) -> Option<LockfileMatch> {
    let manager = if is_root_file(path, active_root, "package-lock.json") {
        "npm"
    } else if is_root_file(path, active_root, "yarn.lock") {
        "yarn"
    } else if is_root_file(path, active_root, "pnpm-lock.yaml") {
        "pnpm"
    } else if is_root_file(path, active_root, "bun.lockb") {
        "bun"
    } else {
        return None;
    };
    Some(LockfileMatch {
        package_manager: manager,
    })
}

pub struct MonorepoToolMatch {
    pub tool_name: Option<&'static str>,
}

pub fn match_monorepo_tool(path: &str, active_root: &str) -> Option<MonorepoToolMatch> {
    if is_root_file(path, active_root, "turbo.json") {
        Some(MonorepoToolMatch {
            tool_name: Some("Turborepo"),
        })
    } else if is_root_file(path, active_root, "nx.json") {
        Some(MonorepoToolMatch {
            tool_name: Some("Nx"),
        })
    } else if is_root_file(path, active_root, "lerna.json") {
        Some(MonorepoToolMatch {
            tool_name: Some("Lerna"),
        })
    } else if is_root_file(path, active_root, "pnpm-workspace.yaml") {
        Some(MonorepoToolMatch { tool_name: None })
    } else {
        None
    }
}

pub fn match_build_tool_config(path: &str, active_root: &str) -> Option<&'static str> {
    let is = |name: &str| is_root_file(path, active_root, name);
    if is("vite.config.ts") || is("vite.config.js") {
        Some("Vite")
    } else if is("webpack.config.ts") || is("webpack.config.js") {
        Some("Webpack")
    } else if is("next.config.js") || is("next.config.ts") || is("next.config.mjs") {
        Some("Next.js")
    } else if is("astro.config.mjs") || is("astro.config.ts") {
        Some("Astro")
    } else {
        None
    }
}

pub fn is_docker_file(path: &str) -> bool {
    path == "Dockerfile" || path.starts_with("docker-compose") || path.starts_with(".docker")
}

pub fn is_ci_file(path: &str) -> bool {
    path.starts_with(".github/workflows/")
        || path == ".gitlab-ci.yml"
        || path.starts_with(".circleci/")
}

pub fn frameworks_from_dependencies(dependencies: &[String]) -> Vec<&'static str> {
    const DEPENDENCY_TO_FRAMEWORK: [(&str, &str); 5] = [
        ("next", "Next.js"),
        ("nuxt", "Nuxt"),
        ("gatsby", "Gatsby"),
        ("astro", "Astro"),
        ("@remix-run/react", "Remix"),
    ];

    DEPENDENCY_TO_FRAMEWORK
        .iter()
        .filter(|(dep_name, _)| dependencies.iter().any(|d| d == dep_name))
        .map(|(_, framework)| *framework)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_lockfiles_at_root_only() {
        assert!(match_lockfile("yarn.lock", "").is_some());
        assert!(match_lockfile("packages/api/yarn.lock", "").is_none());
        assert!(match_lockfile("yarn.lock", "packages/api/").is_none());
        assert!(match_lockfile("packages/api/yarn.lock", "packages/api/").is_some());
    }

    #[test]
    fn detects_frameworks_from_dependency_names() {
        let deps = vec!["react".to_string(), "next".to_string()];
        assert_eq!(frameworks_from_dependencies(&deps), vec!["Next.js"]);
    }
}
