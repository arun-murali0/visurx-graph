mod docker_yaml;
mod entry_point;
pub(crate) mod file_classify;
mod manifest;
mod tooling;

pub use docker_yaml::{DockerComposeData, DockerfileData};
pub use file_classify::FileEntry;
pub use manifest::{PackageJsonData, TsConfigData};

use crate::vfs::Vfs;
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct ScanResult {
    pub total_files: usize,
    pub parseable_count: usize,
    pub skipped_count: usize,
    pub package_manager: Option<String>,
    pub lockfile_path: Option<String>,
    pub is_monorepo: bool,
    pub has_package_json: bool,
    pub package_json_path: Option<String>,
    pub npm_package_name: Option<String>,
    pub entry_point_guess: Option<String>,
    pub parseable_files: Vec<FileEntry>,
    pub non_parseable_files: Vec<FileEntry>,

    pub has_docker: bool,
    pub has_ci: bool,
    pub detected_tools: Vec<String>,

    pub package_data: Option<PackageJsonData>,
    pub tsconfig_path: Option<String>,
    pub tsconfig: Option<TsConfigData>,

    pub dockerfile: Option<DockerfileData>,
    pub docker_compose: Option<DockerComposeData>,
    pub pnpm_workspace_packages: Option<Vec<String>>,
}

fn normalize_root(manual_root: Option<&str>) -> String {
    match manual_root {
        Some(root) if root.ends_with('/') => root.to_string(),
        Some(root) => format!("{root}/"),
        None => String::new(),
    }
}

pub fn scan(vfs: &Vfs, manual_root: Option<&str>) -> ScanResult {
    scan_with_entry_point(vfs, manual_root, None)
}

pub fn scan_with_entry_point(
    vfs: &Vfs,
    manual_root: Option<&str>,
    manual_entry_point: Option<&str>,
) -> ScanResult {
    let active_root = normalize_root(manual_root);

    let estimated_files = 1000;
    let mut parseable_files = Vec::with_capacity(estimated_files);
    let mut non_parseable_files = Vec::with_capacity(estimated_files);
    let mut detected_tools: Vec<String> = Vec::new();

    let mut package_manager: Option<String> = None;
    let mut lockfile_path: Option<String> = None;
    let mut is_monorepo = false;
    let mut has_docker = false;
    let mut has_ci = false;

    let mut package_json_path: Option<String> = None;
    let mut npm_package_name: Option<String> = None;
    let mut package_data: Option<PackageJsonData> = None;

    let mut tsconfig_path: Option<String> = None;
    let mut tsconfig: Option<TsConfigData> = None;

    let mut dockerfile: Option<DockerfileData> = None;
    let mut docker_compose: Option<DockerComposeData> = None;
    let mut pnpm_workspace_packages: Option<Vec<String>> = None;

    for path in vfs.paths() {
        if !active_root.is_empty() && !path.starts_with(&active_root) {
            continue;
        }

        let bytes = vfs.read(path).unwrap_or(&[]);
        let entry = file_classify::classify_file(path, bytes.len() as u32);

        if entry.is_parseable {
            parseable_files.push(entry);
        } else {
            non_parseable_files.push(entry);
        }

        if let Some(lockfile) = tooling::match_lockfile(path, &active_root) {
            lockfile_path = Some(path.to_string());
            package_manager = Some(lockfile.package_manager.to_string());
        }

        if let Some(monorepo_tool) = tooling::match_monorepo_tool(path, &active_root) {
            is_monorepo = true;
            if let Some(name) = monorepo_tool.tool_name {
                detected_tools.push(name.to_string());
            }
        }

        if let Some(build_tool) = tooling::match_build_tool_config(path, &active_root) {
            detected_tools.push(build_tool.to_string());
        }

        if tooling::is_docker_file(path) {
            has_docker = true;
            if let Some(text) = std::str::from_utf8(bytes).ok() {
                if tooling::is_root_file(path, &active_root, "Dockerfile") {
                    dockerfile = Some(docker_yaml::parse_dockerfile(text));
                } else if path.starts_with("docker-compose") {
                    docker_compose = docker_yaml::parse_docker_compose(text).or(docker_compose);
                }
            }
        }
        if tooling::is_ci_file(path) {
            has_ci = true;
        }

        if tooling::is_root_file(path, &active_root, "package.json") {
            package_json_path = Some(path.to_string());
            if let Some(text) = std::str::from_utf8(bytes).ok() {
                npm_package_name = manifest::parse_package_name(text);
                if let Some((data, declares_workspaces)) = manifest::parse_package_json(text) {
                    if declares_workspaces {
                        is_monorepo = true;
                    }
                    detected_tools.extend(
                        tooling::frameworks_from_dependencies(&data.dependencies)
                            .into_iter()
                            .map(String::from),
                    );
                    package_data = Some(data);
                }
            }
        }

        let is_tsconfig_like = tooling::is_root_file(path, &active_root, "tsconfig.json")
            || tooling::is_root_file(path, &active_root, "jsconfig.json");
        if is_tsconfig_like {
            if let Some(parsed) = manifest::parse_tsconfig_resolved(vfs, path) {
                detected_tools.push("TypeScript".to_string());
                tsconfig_path = Some(path.to_string());
                tsconfig = Some(parsed);
            }
        }

        if tooling::is_root_file(path, &active_root, "pnpm-workspace.yaml") {
            if let Some(text) = std::str::from_utf8(bytes).ok() {
                pnpm_workspace_packages = docker_yaml::parse_pnpm_workspace_packages(text);
            }
        }
    }

    detected_tools.sort_unstable();
    detected_tools.dedup();

    let entry_point_guess = entry_point::resolve_entry_point(vfs, &active_root, manual_entry_point);

    ScanResult {
        total_files: parseable_files.len() + non_parseable_files.len(),
        parseable_count: parseable_files.len(),
        skipped_count: non_parseable_files.len(),
        package_manager,
        lockfile_path,
        is_monorepo,
        has_package_json: package_json_path.is_some(),
        package_json_path,
        npm_package_name,
        entry_point_guess,
        parseable_files,
        non_parseable_files,
        has_docker,
        has_ci,
        detected_tools,
        package_data,
        tsconfig_path,
        tsconfig,
        dockerfile,
        docker_compose,
        pnpm_workspace_packages,
    }
}
