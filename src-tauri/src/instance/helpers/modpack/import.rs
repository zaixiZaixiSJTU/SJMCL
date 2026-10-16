use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sjmcl_types::error::SJMCLResult;
use std::fs;
use std::fs::File;
use std::path::Path;
use tauri::AppHandle;
use zip::ZipArchive;

use crate::download::DownloadTask;
use crate::instance::helpers::client_json::{
  ArgumentsItem, DownloadsValue, LibrariesValue, McClientInfo, reset_fields_from_patches,
};
use crate::instance::helpers::modpack::curseforge::CurseForgeManifest;
use crate::instance::helpers::modpack::modrinth::ModrinthManifest;
use crate::instance::helpers::modpack::multimc::MultiMcManifest;
use crate::instance::models::misc::{InstanceError, ModLoader, ModLoaderType};
use crate::resource::commands::fetch_mod_loader_version_list;
use crate::resource::models::OtherResourceSource;

#[async_trait]
pub trait ModpackManifest {
  fn from_archive(file: &File) -> SJMCLResult<Self>
  where
    Self: Sized;
  fn get_client_version(&self) -> SJMCLResult<String>;
  fn get_mod_loader_type_version(&self) -> SJMCLResult<(ModLoaderType, String)>;
  async fn get_meta_info(&self, app: &AppHandle) -> SJMCLResult<ModpackMetaInfo>;
  async fn get_download_params(
    &self,
    app: &AppHandle,
    instance_path: &Path,
  ) -> SJMCLResult<Vec<DownloadTask>>;
  fn get_overrides_path(&self) -> String;
  /// Launch related information declared by the modpack itself, which cannot be
  /// represented by the regular installation pipeline,
  /// e.g. MultiMC/Prism component patches (`patches/*.json`).
  async fn get_launch_info(&self, _app: &AppHandle) -> SJMCLResult<ModpackLaunchInfo> {
    Ok(ModpackLaunchInfo::default())
  }
}

type ManifestBox = Box<dyn ModpackManifest + Send + Sync>;
type Parser = Box<dyn Fn(&File) -> SJMCLResult<ManifestBox> + Send + Sync>;

fn get_parsers() -> Vec<Parser> {
  vec![
    Box::new(|f| {
      CurseForgeManifest::from_archive(f).map(|m| {
        let b: ManifestBox = Box::new(m);
        b
      })
    }),
    Box::new(|f| {
      ModrinthManifest::from_archive(f).map(|m| {
        let b: ManifestBox = Box::new(m);
        b
      })
    }),
    Box::new(|f| {
      MultiMcManifest::from_archive(f).map(|m| {
        let b: ManifestBox = Box::new(m);
        b
      })
    }),
  ]
}

impl ModLoader {
  pub async fn with_branch(&self, app: &AppHandle, mc_version: String) -> SJMCLResult<Self> {
    let version_list =
      fetch_mod_loader_version_list(app.clone(), mc_version, self.loader_type).await?;
    if let Some(version) = version_list.iter().find(|v| v.version == self.version) {
      return Ok(Self {
        branch: version.branch.clone(),
        ..self.clone()
      });
    }
    Err(InstanceError::ModLoaderVersionParseError.into())
  }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModpackMetaInfo {
  pub name: String,
  pub version: Option<String>,
  pub description: Option<String>,
  pub author: Option<String>,
  pub modpack_source: OtherResourceSource,
  pub client_version: String,
  pub mod_loader: Option<ModLoader>,
}

impl ModpackMetaInfo {
  pub async fn from_archive(app: &AppHandle, file: &File) -> SJMCLResult<Self> {
    for parser in get_parsers() {
      if let Ok(manifest) = parser(file) {
        return manifest.get_meta_info(app).await;
      }
    }

    Err(InstanceError::ModpackManifestParseError.into())
  }
}

/// A java agent declared by a modpack component patch (MultiMC `+agents`).
#[derive(Debug, Clone, Default)]
pub struct ModpackAgent {
  /// Path of the agent library relative to the libraries directory.
  pub relative_path: String,
  pub argument: Option<String>,
}

/// A launch configuration declared by a component patch inside the modpack archive,
/// e.g. MultiMC/Prism `patches/<uid>.json`.
#[derive(Debug, Clone, Default)]
pub struct ModpackComponentPatch {
  /// Client json patch id: the mod loader id for mod loader components, the component uid
  /// otherwise.
  pub id: String,
  pub version: Option<String>,
  /// Application order of the patch, ascending.
  pub priority: i64,
  pub main_class: Option<String>,
  pub minecraft_arguments: Option<String>,
  /// The client jar declared by the patch, if any.
  pub main_jar: Option<LibrariesValue>,
  pub libraries: Vec<LibrariesValue>,
  /// Files the component needs in the Maven directory but that are not part of the classpath,
  /// e.g. the installer jar a launch wrapper runs (MultiMC `mavenFiles`).
  pub maven_files: Vec<LibrariesValue>,
  pub jvm_args: Vec<String>,
  pub tweakers: Vec<String>,
  /// MultiMC launch traits, e.g. `FirstThreadOnMacOS`.
  pub traits: Vec<String>,
  pub agents: Vec<ModpackAgent>,
  pub compatible_java_majors: Vec<i32>,
}

/// Launch related information declared by the modpack archive itself, which the regular
/// installation pipeline cannot represent.
#[derive(Debug, Clone, Default)]
pub struct ModpackLaunchInfo {
  pub patches: Vec<ModpackComponentPatch>,
  /// The modpack defines its mod loader completely, so installing the regular mod loader
  /// must be skipped to avoid overwriting it.
  pub is_mod_loader_provided: bool,
  /// Library files shipped inside the archive: (path in archive, path relative to the
  /// libraries directory).
  pub local_libraries: Vec<(String, String)>,
  /// JVM arguments required by the modpack (`instance.cfg` `JvmArgs`).
  pub jvm_args: Option<String>,
  pub max_memory: Option<u32>,
}

pub async fn get_modpack_launch_info(
  app: &AppHandle,
  file: &File,
) -> SJMCLResult<ModpackLaunchInfo> {
  for parser in get_parsers() {
    if let Ok(manifest) = parser(file) {
      return manifest.get_launch_info(app).await;
    }
  }

  Err(InstanceError::ModpackManifestParseError.into())
}

pub fn extract_local_libraries(
  file: &File,
  libraries_dir: &Path,
  local_libraries: &[(String, String)],
) -> SJMCLResult<()> {
  if local_libraries.is_empty() {
    return Ok(());
  }

  let mut archive = ZipArchive::new(file)?;
  for (source, destination) in local_libraries {
    let mut archive_file = archive.by_name(source)?;
    let outpath = libraries_dir.join(destination);

    if let Some(p) = outpath.parent()
      && !p.exists()
    {
      fs::create_dir_all(p)?;
    }

    let mut outfile = File::create(&outpath)?;
    std::io::copy(&mut archive_file, &mut outfile)?;
  }

  Ok(())
}

/// Convert the launch information declared by a modpack into client json patches,
/// so that it takes part in the regular client json merging and downloading.
pub fn apply_launch_info_to_client_info(
  client_info: &mut McClientInfo,
  launch_info: &ModpackLaunchInfo,
) -> SJMCLResult<()> {
  let mut tweakers: Vec<String> = Vec::new();

  for patch in &launch_info.patches {
    let mut patch_info = McClientInfo {
      id: patch.id.clone(),
      version: patch.version.clone(),
      priority: Some(patch.priority),
      main_class: patch.main_class.clone(),
      minecraft_arguments: patch.minecraft_arguments.clone(),
      libraries: patch.libraries.clone(),
      maven_files: patch.maven_files.clone(),
      extra_jvm_arguments: patch.jvm_args.clone(),
      compatible_java_majors: patch.compatible_java_majors.clone(),
      ..Default::default()
    };

    // The client jar declared by the modpack replaces the vanilla one. Jars without a
    // download cannot be fetched and are ignored.
    if let Some(artifact) = patch
      .main_jar
      .as_ref()
      .and_then(|main_jar| main_jar.downloads.as_ref())
      .and_then(|downloads| downloads.artifact.as_ref())
      .filter(|artifact| !artifact.url.is_empty())
    {
      client_info.downloads.insert(
        "client".to_string(),
        DownloadsValue {
          sha1: artifact.sha1.clone(),
          size: artifact.size,
          url: artifact.url.clone(),
        },
      );
    }

    for agent in &patch.agents {
      let mut argument = format!("-javaagent:${{library_directory}}/{}", agent.relative_path);
      if let Some(value) = &agent.argument {
        argument.push('=');
        argument.push_str(value);
      }
      patch_info.extra_jvm_arguments.push(argument);
    }

    // Only the macOS first thread trait affects the launch command, and only on macOS
    // (ref: MultiMC MinecraftInstance::javaArguments).
    if cfg!(target_os = "macos")
      && patch
        .traits
        .iter()
        .any(|trait_| trait_ == "FirstThreadOnMacOS")
    {
      patch_info
        .extra_jvm_arguments
        .push("-XstartOnFirstThread".to_string());
    }

    // If several patches declare the same tweaker, keep only the last declaration,
    // matching MultiMC's tweaker ordering.
    for tweaker in &patch.tweakers {
      if let Some(index) = tweakers.iter().position(|item| item == tweaker) {
        tweakers.remove(index);
      }
      tweakers.push(tweaker.clone());
    }

    client_info.patches.push(patch_info);
  }

  reset_fields_from_patches(client_info);

  // Tweakers are appended to the merged game arguments, no matter which patch declared the
  // arguments themselves.
  if !tweakers.is_empty() {
    append_tweakers(client_info, &tweakers);
  }

  Ok(())
}

/// Append `--tweakClass` arguments to the game arguments of the merged client info.
fn append_tweakers(client_info: &mut McClientInfo, tweakers: &[String]) {
  if let Some(arguments) = &mut client_info.arguments {
    for tweaker in tweakers {
      arguments.game.push(ArgumentsItem {
        value: vec!["--tweakClass".to_string(), tweaker.clone()],
        ..Default::default()
      });
    }
    return;
  }

  let mut arguments = client_info.minecraft_arguments.take().unwrap_or_default();
  for tweaker in tweakers {
    if !arguments.is_empty() && !arguments.ends_with(' ') {
      arguments.push(' ');
    }
    arguments.push_str("--tweakClass ");
    arguments.push_str(tweaker);
  }
  client_info.minecraft_arguments = Some(arguments);
}

pub async fn get_download_params(
  app: &AppHandle,
  file: &File,
  instance_path: &Path,
) -> SJMCLResult<Vec<DownloadTask>> {
  for parser in get_parsers() {
    if let Ok(manifest) = parser(file) {
      return manifest.get_download_params(app, instance_path).await;
    }
  }

  Err(InstanceError::ModpackManifestParseError.into())
}

pub fn extract_overrides(file: &File, instance_path: &Path) -> SJMCLResult<()> {
  let get_overrides_path = |file| {
    for parser in get_parsers() {
      if let Ok(manifest) = parser(file) {
        return Some(manifest.get_overrides_path());
      }
    }
    None
  };
  let overrides_path = get_overrides_path(file).ok_or(InstanceError::ModpackManifestParseError)?;
  let overrides_prefix = format!("{}/", overrides_path.trim_end_matches('/'));
  let mut archive = ZipArchive::new(file)?;
  for i in 0..archive.len() {
    let mut file = archive.by_index(i)?;
    let path = file.mangled_name()?;
    let outpath = if path.starts_with(&overrides_prefix) {
      // Remove "{overrides}/" prefix and join with instance path
      let relative_path = path.strip_prefix(&overrides_prefix).unwrap();
      instance_path.join(relative_path)
    } else {
      continue;
    };

    if file.is_file() {
      // Create parent directories if they don't exist
      if let Some(p) = outpath.parent()
        && !p.exists()
      {
        fs::create_dir_all(p)?;
      }

      // Extract file
      let mut outfile = File::create(&outpath)?;
      std::io::copy(&mut file, &mut outfile)?;
    }
  }
  Ok(())
}
