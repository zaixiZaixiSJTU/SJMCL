use async_trait::async_trait;
use config::Config;
use serde::{Deserialize, Serialize};
use sjmcl_types::error::SJMCLResult;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tauri_plugin_http::reqwest;
use zip::ZipArchive;

use crate::instance::helpers::client_json::{
  DownloadsArtifact, InstructionRule, LibrariesDownloads, LibrariesExtract, LibrariesValue,
};
use crate::instance::helpers::modpack::export::{
  ExportModpackOptions, ModpackExportBundle, normalize_mod_loader_version,
};
use crate::instance::helpers::modpack::import::{
  ModpackAgent, ModpackComponentPatch, ModpackLaunchInfo, ModpackManifest, ModpackMetaInfo,
};
use crate::instance::models::misc::{Instance, InstanceError, ModLoader, ModLoaderType};
use crate::launch::helpers::file_validator::convert_library_name_to_path;
use crate::resource::models::OtherResourceSource;
use crate::tasks::PTaskParam;

/// Component uids whose definition makes the regular mod loader installation unnecessary.
const MOD_LOADER_COMPONENT_UIDS: [&str; 4] = [
  "net.minecraftforge",
  "net.neoforged",
  "net.fabricmc.fabric-loader",
  "org.quiltmc.quilt-loader",
];

/// Component meta server, used to resolve the components that are not shipped with the modpack.
/// Prism's meta is the maintained one and extends the MultiMC format with the Java version
/// declarations (`compatibleJavaMajors`/`compatibleJavaName`) that older packs rely on.
const COMPONENT_META_URL: &str = "https://meta.prismlauncher.org/v1";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MultiMcCacheRequires {
  pub uid: String,
  pub equals: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MultiMcComponent {
  pub cached_name: Option<String>,
  pub cached_requires: Option<Vec<MultiMcCacheRequires>>,
  pub cached_version: Option<String>,
  pub cached_volatile: Option<bool>,
  pub disabled: Option<bool>,
  pub important: Option<bool>,
  pub dependency_only: Option<bool>,
  pub uid: String,
  pub version: Option<String>,
}

/// A library entry inside a component patch.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MultiMcLibrary {
  pub name: String,
  pub downloads: Option<LibrariesDownloads>,
  pub natives: Option<HashMap<String, String>>,
  pub extract: Option<LibrariesExtract>,
  #[serde(default)]
  pub rules: Vec<InstructionRule>,
  /// Maven repository root, used when no explicit download is declared.
  pub url: Option<String>,
  pub sha1: Option<String>,
  pub size: Option<i64>,
  /// MultiMC library hints, e.g. `local`, `always-stale`, `forge-pack-xz`.
  #[serde(rename = "MMC-hint")]
  pub hint: Option<String>,
  #[serde(rename = "MMC-absoluteUrl", alias = "MMC-absulute_url")]
  pub absolute_url: Option<String>,
  #[serde(rename = "MMC-filename")]
  pub filename: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MultiMcAgent {
  pub library: MultiMcLibrary,
  pub argument: Option<String>,
}

/// A component patch file: `patches/<uid>.json`.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MultiMcPatch {
  pub uid: Option<String>,
  /// Legacy alias of `uid`.
  pub file_id: Option<String>,
  pub name: Option<String>,
  pub version: Option<String>,
  /// Application order of the component: components are applied in ascending order (MultiMC
  /// `order`), which may differ from the order in `mmc-pack.json`.
  pub order: Option<i64>,
  pub main_class: Option<String>,
  pub minecraft_arguments: Option<String>,
  #[serde(rename = "+jvmArgs")]
  pub addn_jvm_args: Option<Vec<String>>,
  #[serde(rename = "+tweakers")]
  pub add_tweakers: Option<Vec<String>>,
  /// Launch traits of the component, e.g. FirstThreadOnMacOS.
  #[serde(rename = "+traits")]
  pub add_traits: Option<Vec<String>>,
  #[serde(rename = "+agents")]
  pub agents: Option<Vec<MultiMcAgent>>,
  #[serde(rename = "+libraries")]
  pub addn_libraries: Option<Vec<MultiMcLibrary>>,
  pub libraries: Option<Vec<MultiMcLibrary>>,
  /// Files the component needs in the Maven directory but that are not part of the classpath,
  /// e.g. the installer jar a launch wrapper (ForgeWrapper) runs.
  pub maven_files: Option<Vec<MultiMcLibrary>>,
  /// The client jar of the component.
  pub main_jar: Option<MultiMcLibrary>,
  /// Prism extension: the exact set of Java major versions this component supports.
  pub compatible_java_majors: Option<Vec<i32>>,
}

structstruck::strike! {
#[strikethrough[derive(Deserialize, Serialize, Debug, Clone)]]
#[strikethrough[serde(rename_all = "camelCase")]]
  pub struct MultiMcManifest {
    pub components: Vec<MultiMcComponent>,
    pub format_version: u64,
    #[serde(skip)]
    pub cfg: HashMap<String, String>,
    #[serde(skip)]
    pub base_path: String,
    /// Component patches stored in `patches/<uid>.json`, keyed by component uid.
    #[serde(skip)]
    pub patches: HashMap<String, MultiMcPatch>,
    #[serde(skip)]
    pub launch_info: ModpackLaunchInfo,
  }
}

#[async_trait]
impl ModpackManifest for MultiMcManifest {
  fn from_archive(file: &File) -> SJMCLResult<Self> {
    let mut archive = ZipArchive::new(file)?;

    let base_path = if archive.by_name("mmc-pack.json").is_ok() {
      String::new()
    } else {
      let mut found_path = None;
      for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file.name()?;

        if name.ends_with("mmc-pack.json")
          && let Some(last_slash) = name.rfind('/')
        {
          let dir_path = &name[..=last_slash];
          let depth = name[..last_slash].matches('/').count();
          if depth <= 1 {
            found_path = Some(dir_path.to_string());
            break;
          }
        }
      }
      found_path.ok_or(InstanceError::ModpackManifestParseError)?
    };

    let mut manifest: MultiMcManifest;
    {
      let manifest_path = format!("{}mmc-pack.json", base_path);
      let mut manifest_file = archive.by_name(&manifest_path)?;
      let mut manifest_content = String::new();
      manifest_file.read_to_string(&mut manifest_content)?;
      manifest = serde_json::from_str(&manifest_content)?;
    }

    let cfg_path = format!("{}instance.cfg", base_path);
    let mut cfg_str = String::new();
    {
      let mut cfg_file = archive.by_name(&cfg_path)?;
      cfg_file.read_to_string(&mut cfg_str)?;
    }

    let config = Config::builder()
      .add_source(config::File::from_str(&cfg_str, config::FileFormat::Ini))
      .build()?;

    manifest.base_path = base_path;
    manifest.cfg = config.try_deserialize::<HashMap<String, String>>()?;
    manifest.patches = read_patches(&mut archive, &manifest.base_path)?;
    let launch_info = build_launch_info(&mut archive, &manifest)?;
    manifest.launch_info = launch_info;

    Ok(manifest)
  }

  async fn get_meta_info(&self, app: &AppHandle) -> SJMCLResult<ModpackMetaInfo> {
    let client_version = self.get_client_version()?;
    let mod_loader = if let Ok((loader_type, version)) = self.get_mod_loader_type_version() {
      let mod_loader = ModLoader {
        loader_type,
        version,
        ..Default::default()
      };
      if self.launch_info.is_mod_loader_provided {
        // The modpack ships the complete definition of its mod loader itself, so the loader
        // version does not need to be present in SJMCL's mod loader version list.
        Some(mod_loader)
      } else {
        Some(mod_loader.with_branch(app, client_version.clone()).await?)
      }
    } else {
      None
    };
    Ok(ModpackMetaInfo {
      name: self.cfg.get("name").cloned().unwrap_or_default(),
      version: None,
      description: None,
      author: None,
      modpack_source: OtherResourceSource::MultiMc,
      client_version,
      mod_loader,
    })
  }

  fn get_client_version(&self) -> SJMCLResult<String> {
    let component = self
      .components
      .iter()
      .find(|component| component.uid == "net.minecraft")
      .ok_or(InstanceError::ModpackManifestParseError)?;

    get_version(component)
  }

  fn get_mod_loader_type_version(&self) -> SJMCLResult<(ModLoaderType, String)> {
    for component in &self.components {
      if component.disabled.unwrap_or(false) {
        continue;
      }
      match component.uid.as_str() {
        "net.minecraft" => continue,
        // Cleanroom packs keep the Forge component uid, but replace its startup entry point
        // and libraries, so they must not be treated as regular Forge.
        "net.minecraftforge" => {
          let loader_type = if self.is_cleanroom_component(component) {
            ModLoaderType::Cleanroom
          } else {
            ModLoaderType::Forge
          };
          return Ok((loader_type, get_version(component)?));
        }
        "net.fabricmc.fabric-loader" => {
          return Ok((ModLoaderType::Fabric, get_version(component)?));
        }
        "net.neoforged" => return Ok((ModLoaderType::NeoForge, get_version(component)?)),
        _ => continue,
      }
    }
    Err(InstanceError::ModpackManifestParseError.into())
  }

  async fn get_download_params(
    &self,
    _app: &AppHandle,
    _instance_path: &Path,
  ) -> SJMCLResult<Vec<PTaskParam>> {
    // MultiMC Manifests do not include download parameters
    Ok(Vec::new())
  }

  fn get_overrides_path(&self) -> String {
    format!("{}.minecraft/", self.base_path)
  }

  async fn get_launch_info(&self, app: &AppHandle) -> SJMCLResult<ModpackLaunchInfo> {
    let mut launch_info = self.launch_info.clone();

    // Components without a patch inside the archive are resolved from the component meta
    // server, like MultiMC and Prism do themselves. Components that only override the game
    // itself take part too, since they declare the Java versions the pack supports.
    let client = app.state::<reqwest::Client>();
    for (index, component) in self.components.iter().enumerate() {
      if component.disabled.unwrap_or(false) || self.patches.contains_key(&component.uid) {
        continue;
      }
      let Ok(version) = get_version(component) else {
        continue;
      };

      let patch = match fetch_component_patch(&client, &component.uid, &version).await {
        Ok(patch) => patch,
        Err(_) => {
          log::warn!(
            "Failed to fetch metadata of modpack component {} {}",
            component.uid,
            version
          );
          continue;
        }
      };

      // Metadata components cannot ship local libraries, so every library must be downloadable.
      let order = patch.order.unwrap_or(index as i64);
      let mut find_local = |_relative_path: &str| None;
      let converted = convert_component_patch(component, &patch, order, false, &mut find_local);
      accept_component_patch(&mut launch_info, &component.uid, converted);
    }

    Ok(launch_info)
  }
}

fn get_version(component: &MultiMcComponent) -> SJMCLResult<String> {
  component
    .version
    .as_ref()
    .or(component.cached_version.as_ref())
    .cloned()
    .ok_or(InstanceError::ModpackManifestParseError.into())
}

pub fn build_multimc_export_bundle(
  instance: &Instance,
  options: &ExportModpackOptions,
  selected_files: &[(String, PathBuf)],
) -> SJMCLResult<ModpackExportBundle> {
  let manifest = generate_multimc_manifest(instance);
  let json = serde_json::to_string_pretty(&manifest)
    .map_err(|_| InstanceError::ModpackManifestParseError)?;

  Ok(ModpackExportBundle {
    overrides_prefix: ".minecraft".to_string(),
    overrides_files: selected_files.to_vec(),
    extra_files: vec![
      ("mmc-pack.json".to_string(), json),
      (
        "instance.cfg".to_string(),
        generate_multimc_instance_cfg(options),
      ),
      (".packignore".to_string(), String::new()),
    ],
  })
}

fn generate_multimc_manifest(instance: &Instance) -> MultiMcManifest {
  let mut components = Vec::new();
  components.push(MultiMcComponent {
    uid: "net.minecraft".to_string(),
    version: Some(instance.version.clone()),
    important: Some(true),
    dependency_only: Some(false),
    cached_name: None,
    cached_requires: None,
    cached_version: None,
    cached_volatile: None,
    disabled: None,
  });

  if instance.mod_loader.loader_type != ModLoaderType::Unknown {
    let version = normalize_mod_loader_version(&instance.mod_loader.version);
    let uid = match instance.mod_loader.loader_type {
      ModLoaderType::Forge | ModLoaderType::LegacyForge => "net.minecraftforge",
      ModLoaderType::NeoForge => "net.neoforged",
      ModLoaderType::Fabric => "net.fabricmc.fabric-loader",
      ModLoaderType::Quilt => "org.quiltmc.quilt-loader",
      _ => "",
    };

    if !uid.is_empty() {
      components.push(MultiMcComponent {
        uid: uid.to_string(),
        version: Some(version),
        important: Some(false),
        dependency_only: Some(false),
        cached_name: None,
        cached_requires: None,
        cached_version: None,
        cached_volatile: None,
        disabled: None,
      });
    }
  }

  MultiMcManifest {
    format_version: 1,
    components,
    cfg: HashMap::new(),
    base_path: String::new(),
    patches: HashMap::new(),
    launch_info: ModpackLaunchInfo::default(),
  }
}

fn generate_multimc_instance_cfg(options: &ExportModpackOptions) -> String {
  let mut content = format!(
    "# Auto generated by SJMC Launcher\nInstanceType=OneSix\nname={}\n",
    options.name
  );
  if let Some(min_memory) = options.min_memory {
    content.push_str("OverrideMemory=true\n");
    content.push_str(&format!("MinMemAlloc={}\n", min_memory));
  }
  content
}

impl MultiMcManifest {
  fn is_cleanroom_component(&self, component: &MultiMcComponent) -> bool {
    let is_cleanroom = |name: &str| name.to_lowercase().contains("cleanroom");

    self
      .patches
      .get(&component.uid)
      .and_then(|patch| patch.name.as_deref())
      .is_some_and(is_cleanroom)
      || component.cached_name.as_deref().is_some_and(is_cleanroom)
  }
}

/// Read all component patches (`patches/<uid>.json`) stored in the archive.
fn read_patches<R: Read + Seek>(
  archive: &mut ZipArchive<R>,
  base_path: &str,
) -> SJMCLResult<HashMap<String, MultiMcPatch>> {
  let patches_prefix = format!("{}patches/", base_path);
  let mut patch_names = Vec::new();
  for i in 0..archive.len() {
    let name = archive.by_index(i)?.name()?.to_string();
    if name.starts_with(&patches_prefix) && name.ends_with(".json") {
      patch_names.push(name);
    }
  }

  let mut patches = HashMap::new();
  for name in patch_names {
    let mut content = String::new();
    archive.by_name(&name)?.read_to_string(&mut content)?;
    let patch: MultiMcPatch = serde_json::from_str(&content)?;

    // The file name must equal the uid, but an explicit uid inside the file takes precedence.
    let uid = patch.uid.clone().or(patch.file_id.clone()).or_else(|| {
      name
        .rsplit('/')
        .next()
        .map(|file_name| file_name.trim_end_matches(".json").to_string())
    });
    if let Some(uid) = uid {
      patches.insert(uid, patch);
    }
  }

  Ok(patches)
}

/// Build the launch information declared by the patches shipped inside the modpack archive.
/// Only the components listed in `mmc-pack.json` are applied, following the order of that list.
fn build_launch_info<R: Read + Seek>(
  archive: &mut ZipArchive<R>,
  manifest: &MultiMcManifest,
) -> SJMCLResult<ModpackLaunchInfo> {
  let mut launch_info = ModpackLaunchInfo::default();
  let base_path = manifest.base_path.clone();

  for (index, component) in manifest.components.iter().enumerate() {
    if component.disabled.unwrap_or(false) {
      continue;
    }
    let Some(patch) = manifest.patches.get(&component.uid) else {
      continue;
    };

    let is_cleanroom = manifest.is_cleanroom_component(component);
    let order = patch.order.unwrap_or(index as i64);
    let converted = {
      let mut find_local =
        |relative_path: &str| find_library_in_archive(archive, &base_path, relative_path);
      convert_component_patch(component, patch, order, is_cleanroom, &mut find_local)
    };

    accept_component_patch(&mut launch_info, &component.uid, converted);
  }

  if is_cfg_enabled(&manifest.cfg, "OverrideJavaArgs") {
    launch_info.jvm_args = manifest
      .cfg
      .get("JvmArgs")
      .map(|args| args.trim().to_string())
      .filter(|args| !args.is_empty());
  }
  if is_cfg_enabled(&manifest.cfg, "OverrideMemory") {
    launch_info.max_memory = get_cfg_u32(&manifest.cfg, "MaxMemAlloc");
  }

  Ok(launch_info)
}

/// Keep a converted component in the launch information. A mod loader component is only kept
/// when it defines the mod loader completely, so that a partial definition does not hide the
/// regular mod loader installation of SJMCL.
fn accept_component_patch(
  launch_info: &mut ModpackLaunchInfo,
  uid: &str,
  converted: ConvertedComponentPatch,
) {
  if MOD_LOADER_COMPONENT_UIDS.contains(&uid) {
    if converted.patch.libraries.is_empty() || !converted.is_complete {
      // The component does not define the mod loader completely, so the regular mod
      // loader installation is kept instead of applying a partial definition.
      log::warn!(
        "Modpack component {} is incomplete, falling back to the regular mod loader installation",
        uid
      );
      return;
    }
    launch_info.is_mod_loader_provided = true;
  }

  launch_info.patches.push(converted.patch);
  launch_info
    .local_libraries
    .extend(converted.local_libraries);
}

/// One component patch converted into client json form.
struct ConvertedComponentPatch {
  patch: ModpackComponentPatch,
  /// Library files shipped inside the archive: (path in archive, path relative to the
  /// libraries directory).
  local_libraries: Vec<(String, String)>,
  /// Whether every library declared by the patch can be obtained.
  is_complete: bool,
}

/// Application priority of a component patch. Every patch is applied after the base game patch,
/// and patches follow the application order declared by their component (`order`, falling back
/// to the position of the component in `mmc-pack.json`).
fn component_priority(order: i64) -> i64 {
  1000i64.saturating_add(order.max(-999))
}

/// Resolve one library declared by a component patch. Returns the library and its path
/// relative to the libraries directory when it can be obtained; libraries shipped inside the
/// archive are recorded in `local_libraries`.
fn resolve_library(
  library: &MultiMcLibrary,
  uid: &str,
  find_local: &mut dyn FnMut(&str) -> Option<String>,
  local_libraries: &mut Vec<(String, String)>,
) -> Option<(LibrariesValue, Option<String>)> {
  let (library_value, relative_path) = convert_library(library);
  // Native libraries may only carry classifier downloads instead of an artifact.
  let has_url = library_value.downloads.as_ref().is_some_and(|downloads| {
    downloads
      .artifact
      .iter()
      .chain(
        downloads
          .classifiers
          .iter()
          .flat_map(|classifiers| classifiers.values()),
      )
      .any(|artifact| !artifact.url.is_empty())
  });

  // Native libraries without any download are not fetched at all, so they are always usable.
  let mut resolvable =
    has_url || (library_value.natives.is_some() && library_value.downloads.is_none());
  if (library.hint.as_deref() == Some("local") || !resolvable)
    && let Some(relative_path) = relative_path.as_deref()
    && let Some(source) = find_local(&library_source_path(library, relative_path))
  {
    // Libraries without a download, or marked `local`, are shipped with the modpack.
    local_libraries.push((source, relative_path.to_string()));
    resolvable = true;
  }

  if !resolvable {
    // The library cannot be obtained. Keep the rest of the definition instead of failing
    // the whole import; if it belongs to a mod loader, the regular installation handles it.
    log::warn!(
      "Skipping unavailable library {} of modpack component {}",
      library.name,
      uid
    );
    return None;
  }

  Some((library_value, relative_path))
}

/// Convert one component patch into a client json patch.
/// `find_local` returns the path inside the archive of a library shipped with the modpack.
/// `order` is the application order of the component, see `component_priority`.
fn convert_component_patch(
  component: &MultiMcComponent,
  patch: &MultiMcPatch,
  order: i64,
  is_cleanroom: bool,
  find_local: &mut dyn FnMut(&str) -> Option<String>,
) -> ConvertedComponentPatch {
  let mut libraries = Vec::new();
  let mut maven_files = Vec::new();
  let mut local_libraries = Vec::new();
  let mut agents = Vec::new();
  let mut is_complete = true;

  // (library, is used by an agent, agent argument)
  let mut entries: Vec<(&MultiMcLibrary, bool, Option<String>)> = Vec::new();
  for library in patch
    .libraries
    .iter()
    .flatten()
    .chain(patch.addn_libraries.iter().flatten())
  {
    entries.push((library, false, None));
  }
  for agent in patch.agents.iter().flatten() {
    entries.push((&agent.library, true, agent.argument.clone()));
  }

  for (library, is_agent, agent_argument) in entries {
    let Some((library_value, relative_path)) =
      resolve_library(library, &component.uid, find_local, &mut local_libraries)
    else {
      is_complete = false;
      continue;
    };

    libraries.push(library_value);

    if is_agent && let Some(relative_path) = relative_path {
      agents.push(ModpackAgent {
        relative_path,
        argument: agent_argument,
      });
    }
  }

  // Files that must be present in the Maven directory but that are not part of the classpath.
  for library in patch.maven_files.iter().flatten() {
    let Some((library_value, _)) =
      resolve_library(library, &component.uid, find_local, &mut local_libraries)
    else {
      is_complete = false;
      continue;
    };
    maven_files.push(library_value);
  }

  ConvertedComponentPatch {
    patch: ModpackComponentPatch {
      id: patch_id(component, is_cleanroom),
      version: get_version(component)
        .ok()
        .or_else(|| patch.version.clone()),
      priority: component_priority(order),
      main_class: patch.main_class.clone(),
      minecraft_arguments: patch.minecraft_arguments.clone(),
      main_jar: patch
        .main_jar
        .as_ref()
        .map(|library| convert_library(library).0),
      libraries,
      replaces_libraries: patch.libraries.is_some(),
      maven_files,
      jvm_args: patch.addn_jvm_args.clone().unwrap_or_default(),
      tweakers: patch.add_tweakers.clone().unwrap_or_default(),
      traits: patch.add_traits.clone().unwrap_or_default(),
      agents,
      compatible_java_majors: patch.compatible_java_majors.clone().unwrap_or_default(),
    },
    local_libraries,
    is_complete,
  }
}

/// Client json patch id of a component. Mod loader components use the id of the corresponding
/// mod loader, so that the regular mod loader handling keeps working on imported instances.
fn patch_id(component: &MultiMcComponent, is_cleanroom: bool) -> String {
  let id = match component.uid.as_str() {
    "net.minecraftforge" if is_cleanroom => "cleanroom",
    "net.minecraftforge" => "forge",
    "net.neoforged" => "neoforge",
    "net.fabricmc.fabric-loader" => "fabric",
    "org.quiltmc.quilt-loader" => "quilt",
    _ => return component.uid.clone(),
  };
  id.to_string()
}

/// Convert a MultiMC library entry into an SJMCL one, and return its path relative to the
/// libraries directory when it can be derived from the library name.
fn convert_library(library: &MultiMcLibrary) -> (LibrariesValue, Option<String>) {
  let relative_path = convert_library_name_to_path(&library.name, None).ok();
  let mut downloads = library.downloads.clone();

  if let Some(classifiers) = downloads
    .as_mut()
    .and_then(|downloads| downloads.classifiers.as_mut())
  {
    for (classifier, artifact) in classifiers {
      if artifact.path.is_empty()
        && let Ok(path) = convert_library_name_to_path(&library.name, Some(classifier.clone()))
      {
        artifact.path = path;
      }
    }
  }

  let declared_path = downloads
    .as_ref()
    .and_then(|downloads| downloads.artifact.as_ref())
    .map(|artifact| artifact.path.clone())
    .filter(|path| !path.is_empty());
  let declared_sha1 = downloads
    .as_ref()
    .and_then(|downloads| downloads.artifact.as_ref())
    .map(|artifact| artifact.sha1.clone())
    .filter(|sha1| !sha1.is_empty());
  let declared_size = downloads
    .as_ref()
    .and_then(|downloads| downloads.artifact.as_ref())
    .map(|artifact| artifact.size)
    .filter(|size| *size > 0);
  let declared_url = downloads
    .as_ref()
    .and_then(|downloads| downloads.artifact.as_ref())
    .map(|artifact| artifact.url.clone())
    .filter(|url| !url.is_empty())
    .or_else(|| library.absolute_url.clone().filter(|url| !url.is_empty()));

  let download_url = declared_url.or_else(|| {
    if library.hint.as_deref() == Some("local") {
      return None;
    }
    // MultiMC uses the Minecraft library repository when no URL is declared.
    let repository = library
      .url
      .as_deref()
      .unwrap_or("https://libraries.minecraft.net")
      .trim_end_matches('/');
    let relative_path = relative_path.as_ref()?;
    Some(format!("{}/{}", repository, relative_path))
  });

  if let Some(url) = download_url
    && let Some(path) = declared_path.or_else(|| relative_path.clone())
  {
    downloads.get_or_insert_default().artifact = Some(DownloadsArtifact {
      path,
      url,
      sha1: library.sha1.clone().or(declared_sha1).unwrap_or_default(),
      size: library.size.or(declared_size).unwrap_or_default(),
    });
  }

  (
    LibrariesValue {
      name: library.name.clone(),
      downloads,
      natives: library.natives.clone(),
      extract: library.extract.clone(),
      rules: library.rules.clone(),
    },
    relative_path,
  )
}

/// Path of a library inside the modpack archive: `MMC-filename` replaces the file name
/// derived from the library name.
fn library_source_path(library: &MultiMcLibrary, relative_path: &str) -> String {
  let Some(filename) = library.filename.as_ref().filter(|name| !name.is_empty()) else {
    return relative_path.to_string();
  };

  match relative_path.rfind('/') {
    Some(index) => format!("{}/{}", &relative_path[..index], filename),
    None => filename.clone(),
  }
}

fn find_library_in_archive<R: Read + Seek>(
  archive: &mut ZipArchive<R>,
  base_path: &str,
  relative_path: &str,
) -> Option<String> {
  for prefix in [format!("{}libraries/", base_path), "libraries/".to_string()] {
    let path = format!("{}{}", prefix, relative_path);
    if archive.by_name(&path).is_ok() {
      return Some(path);
    }
    // MultiMC also stores local libraries directly in libraries/ while their
    // classpath destination still follows the Maven layout.
    if let Some(filename) = relative_path.rsplit('/').next() {
      let path = format!("{}{}", prefix, filename);
      if archive.by_name(&path).is_ok() {
        return Some(path);
      }
    }
  }
  None
}

/// Fetch the metadata of a component from the MultiMC meta server, in the same version file
/// format as the patches shipped inside the modpack archive.
async fn fetch_component_patch(
  client: &reqwest::Client,
  uid: &str,
  version: &str,
) -> SJMCLResult<MultiMcPatch> {
  let url = format!("{}/{}/{}.json", COMPONENT_META_URL, uid, version);
  client
    .get(&url)
    .send()
    .await
    .map_err(|_| InstanceError::NetworkError)?
    .json::<MultiMcPatch>()
    .await
    .map_err(|_| InstanceError::ModpackManifestParseError.into())
}

fn is_cfg_enabled(cfg: &HashMap<String, String>, key: &str) -> bool {
  cfg
    .get(key)
    .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn get_cfg_u32(cfg: &HashMap<String, String>, key: &str) -> Option<u32> {
  cfg
    .get(key)
    .and_then(|value| value.trim().parse::<u32>().ok())
    .filter(|value| *value > 0)
}
