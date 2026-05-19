//! Agent creation commands — create new agent skeleton and install it

use std::io::Write;

use serde::Serialize;
use tauri::State;

use crate::state::AppState;

/// Serializable agent manifest for safe TOML generation.
/// Uses proper serialization to avoid TOML injection from user input.
#[derive(Serialize)]
struct AgentManifest<'a> {
    package: PackageSection<'a>,
    llm: LlmSection<'a>,
}

#[derive(Serialize)]
struct PackageSection<'a> {
    agent_id: &'a str,
    name: &'a str,
    version: &'a str,
    description: &'a str,
    author: &'a str,
    runtime_version: &'a str,
    dev: bool,
}

#[derive(Serialize)]
struct LlmSection<'a> {
    suggested_provider: &'a str,
    suggested_model: &'a str,
}

/// Create a new agent skeleton, zip it, and install via Gateway.
///
/// Returns the agent_id of the newly installed agent on success.
#[tauri::command]
pub async fn create_agent(
    state: State<'_, AppState>,
    agent_id: String,
    name: String,
    version: Option<String>,
    description: Option<String>,
    author: Option<String>,
    suggested_provider: Option<String>,
    suggested_model: Option<String>,
) -> Result<String, String> {
    let version = version.unwrap_or_else(|| "0.1.0".to_string());
    let description = description.unwrap_or_else(|| format!("{} agent", name));
    let author = author.unwrap_or_else(|| "RollBall User".to_string());
    let provider = suggested_provider.unwrap_or_else(|| "openai".to_string());
    let model = suggested_model.unwrap_or_else(|| "gpt-4o".to_string());

    // Create a temp directory for the skeleton (use monotonic timestamp to avoid
    // collisions when concurrent Tauri commands run in the same process).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!(
        "rollball-create-{}-{:x}",
        agent_id.replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '-', "_"),
        nanos,
    ));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp dir: {}", e))?;

    // Create prompts/ directory
    let prompts_dir = temp_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .map_err(|e| format!("Failed to create prompts dir: {}", e))?;

    // Generate manifest.toml via safe TOML serialization
    let manifest = AgentManifest {
        package: PackageSection {
            agent_id: &agent_id,
            name: &name,
            version: &version,
            description: &description,
            author: &author,
            runtime_version: "0.1.0",
            dev: true,
        },
        llm: LlmSection {
            suggested_provider: &provider,
            suggested_model: &model,
        },
    };

    let manifest_toml = toml::to_string(&manifest)
        .map_err(|e| format!("Failed to serialize manifest: {}", e))?;

    std::fs::write(temp_dir.join("manifest.toml"), &manifest_toml)
        .map_err(|e| format!("Failed to write manifest: {}", e))?;

    // Generate default system prompt
    let system_prompt = format!(
        "You are {}, an AI assistant.\n\n\
        Role: {}\n\n\
        You can use available tools to help users complete tasks. \
        Always be helpful, accurate, and concise.\n\n\
        When using tools:\n\
        - Explain what you're doing before calling a tool\n\
        - Report the results clearly\n\
        - Handle errors gracefully\n\n\
        If you encounter a problem you cannot solve, \
        explain what you've tried and suggest alternatives.\n",
        name, description,
    );

    std::fs::write(prompts_dir.join("system.md"), &system_prompt)
        .map_err(|e| format!("Failed to write system prompt: {}", e))?;

    // Generate config/settings.toml
    let config_dir = temp_dir.join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir: {}", e))?;
    std::fs::write(
        config_dir.join("settings.toml"),
        "# Agent settings\n",
    )
    .map_err(|e| format!("Failed to write settings: {}", e))?;

    // Zip the skeleton directory into a temporary .agent file
    let zip_path = temp_dir.with_extension("agent");
    let zip_file = std::fs::File::create(&zip_path)
        .map_err(|e| format!("Failed to create zip file: {}", e))?;
    let mut zip_writer = zip::ZipWriter::new(zip_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Recursively add files from temp_dir to the zip
    add_dir_to_zip(&mut zip_writer, &temp_dir, &temp_dir, options)
        .map_err(|e| format!("Failed to zip skeleton: {}", e))?;

    zip_writer
        .finish()
        .map_err(|e| format!("Failed to finalize zip: {}", e))?;

    // Read the zip bytes
    let package_bytes = std::fs::read(&zip_path)
        .map_err(|e| format!("Failed to read zip: {}", e))?;

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&temp_dir);
    let _ = std::fs::remove_file(&zip_path);

    // Install via Gateway
    let client = state.gateway.read().await;
    client
        .install_agent(&package_bytes, true)
        .await
        .map_err(|e| format!("Install failed: {}", e))?;

    Ok(agent_id)
}

/// Recursively add a directory's contents to a zip archive
fn add_dir_to_zip<W: Write + std::io::Seek>(
    zip_writer: &mut zip::ZipWriter<W>,
    base_dir: &std::path::Path,
    current_dir: &std::path::Path,
    options: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    for entry in std::fs::read_dir(current_dir)
        .map_err(|e| format!("Failed to read dir {:?}: {}", current_dir, e))?
    {
        let entry = entry.map_err(|e| format!("Dir entry error: {}", e))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(base_dir)
            .map_err(|e| format!("Strip prefix error: {}", e))?;

        if path.is_dir() {
            // Add directory entry
            let dir_path = relative.to_string_lossy().replace('\\', "/");
            zip_writer
                .add_directory(format!("{}/", dir_path), options)
                .map_err(|e| format!("Failed to add dir '{}': {}", dir_path, e))?;
            add_dir_to_zip(zip_writer, base_dir, &path, options)?;
        } else if path.is_file() {
            let file_path = relative.to_string_lossy().replace('\\', "/");
            zip_writer
                .start_file(file_path.as_str(), options)
                .map_err(|e| format!("Failed to start file '{}': {}", file_path, e))?;
            let content = std::fs::read(&path)
                .map_err(|e| format!("Failed to read file {:?}: {}", path, e))?;
            zip_writer
                .write_all(&content)
                .map_err(|e| format!("Failed to write file '{}': {}", file_path, e))?;
        }
    }
    Ok(())
}
