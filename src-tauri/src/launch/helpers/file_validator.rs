use futures::future::join_all;
use futures::stream::{self, StreamExt, TryStreamExt};
use semver::Version;
use sjmcl_types::error::{SJMCLError, SJMCLResult};
use sjmcl_types::storage::load_json_async;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use tauri::AppHandle;
use tokio::fs;
use zip::ZipArchive;

#[cfg(target_os = "windows")]
use std::sync::Mutex;
#[cfg(target_os = "windows")]
use tauri::Manager;

use crate::instance::helpers::asset_index::AssetIndex;
use crate::instance::helpers::asset_index::load_asset_index;
use crate::instance::helpers::client_json::{
  DownloadsArtifact, FeaturesInfo, IsAllowed, LibrariesValue, McClientInfo,
};
use crate::instance::models::misc::{InstanceError, ModLoaderType};
use crate::launch::helpers::misc::get_natives_string;
use crate::launch::models::LaunchError;
use crate::resource::helpers::misc::{convert_url_to_target_source, get_download_api};
use crate::resource::models::{ResourceType, SourceType};
use crate::tasks::PTaskParam;
use crate::tasks::download::DownloadParam;
use crate::utils::fs::validate_sha1;

#[cfg(target_os = "windows")]
use crate::instance::helpers::client_json::load_native_libraries_replace_map;
#[cfg(target_os = "windows")]
use crate::launcher_config::helpers::graphics::mesa_driver_name;
#[cfg(target_os = "windows")]
use crate::launcher_config::models::{GameConfig, LauncherConfig};

pub fn get_nonnative_library_artifacts(client_info: &McClientInfo) -> Vec<DownloadsArtifact> {
  let mut artifacts = HashSet::new();
  let feature = FeaturesInfo::default();
  for library in &client_info.libraries {
    if !library.is_allowed(&feature).unwrap_or(false) {
      continue;
    }
    if library.natives.is_some() {
      continue;
    }
    if let Some(downloads) = &library.downloads
      && let Some(artifact) = &downloads.artifact
    {
      artifacts.insert(artifact.clone());
    }
  }
  artifacts.into_iter().collect()
}

pub fn get_native_library_artifacts(client_info: &McClientInfo) -> Vec<DownloadsArtifact> {
  let mut artifacts = HashSet::new();
  let feature = FeaturesInfo::default();

  for library in &client_info.libraries {
    if !library.is_allowed(&feature).unwrap_or(false) {
      continue;
    }
    if let Some(natives) = &library.natives {
      if let Some(native) = get_natives_string(natives) {
        if let Some(downloads) = &library.downloads
          && let Some(classifiers) = &downloads.classifiers
          && let Some(artifact) = classifiers.get(&native)
        {
          artifacts.insert(artifact.clone());
        }
      } else {
        println!("natives is None");
      }
    }
  }
  artifacts.into_iter().collect()
}

/// Files that must be present in the Maven directory but that are not part of the classpath,
/// e.g. the installer jar a modpack component's launch wrapper runs.
pub fn get_maven_file_artifacts(client_info: &McClientInfo) -> Vec<DownloadsArtifact> {
  let mut artifacts = HashSet::new();
  for library in &client_info.maven_files {
    if let Some(artifact) = library
      .downloads
      .as_ref()
      .and_then(|downloads| downloads.artifact.as_ref())
    {
      artifacts.insert(artifact.clone());
    }
  }
  artifacts.into_iter().collect()
}

pub async fn get_invalid_library_files(
  source: SourceType,
  library_path: &Path,
  client_info: &McClientInfo,
  check_hash: bool,
) -> SJMCLResult<Vec<PTaskParam>> {
  let mut artifacts = HashSet::new();
  artifacts.extend(get_native_library_artifacts(client_info));
  artifacts.extend(get_nonnative_library_artifacts(client_info));
  artifacts.extend(get_maven_file_artifacts(client_info));

  let futs = artifacts.into_iter().map(move |artifact| async move {
    let file_path = library_path.join(&artifact.path);
    let exists = fs::try_exists(&file_path).await?;
    // Libraries declared by a modpack may not carry a checksum, and an empty one means
    // there is nothing to validate.
    let has_sha1 = !artifact.sha1.is_empty();
    if exists
      && (!check_hash
        || !has_sha1
        || validate_sha1(file_path.clone(), artifact.sha1.clone()).is_ok())
    {
      Ok(None)
    } else if artifact.url.is_empty() {
      Err(LaunchError::GameFilesIncomplete.into())
    } else {
      let src = convert_url_to_target_source(
        &url::Url::parse(&artifact.url)?,
        &[
          ResourceType::Libraries,
          ResourceType::FabricMaven,
          ResourceType::ForgeMaven,
          ResourceType::ForgeMavenNew,
          ResourceType::NeoforgeMaven,
        ],
        &source,
      )?;
      Ok(Some(PTaskParam::Download(DownloadParam {
        src,
        dest: file_path,
        filename: None,
        sha1: has_sha1.then(|| artifact.sha1.clone()),
      })))
    }
  });

  let results: Vec<SJMCLResult<Option<PTaskParam>>> = join_all(futs).await;

  let mut params = Vec::new();
  for r in results {
    if let Some(p) = r? {
      params.push(p);
    }
  }

  Ok(params)
}

#[cfg(target_os = "windows")]
pub fn get_windows_mesa_loader_library(app: &AppHandle) -> SJMCLResult<Option<LibrariesValue>> {
  let all_replace_map = load_native_libraries_replace_map(app)?;
  let (os, arch) = {
    let launcher_config_state = app.state::<Mutex<LauncherConfig>>();
    let cfg = launcher_config_state.lock().unwrap();
    (cfg.basic_info.os_type.clone(), cfg.basic_info.arch.clone())
  };
  let platform_key = format!("{}-{}", os.to_lowercase(), arch.to_lowercase());
  Ok(
    all_replace_map
      .get(platform_key.as_str())
      .and_then(|platform_map| platform_map.get("mesa-loader"))
      .cloned()
      .flatten(),
  )
}

#[cfg(target_os = "windows")]
pub fn get_windows_mesa_loader_path(
  app: &AppHandle,
  library_path: &Path,
) -> SJMCLResult<Option<PathBuf>> {
  Ok(get_windows_mesa_loader_library(app)?.and_then(|lib| {
    lib
      .downloads
      .and_then(|downloads| downloads.artifact)
      .map(|artifact| library_path.join(artifact.path))
  }))
}

#[cfg(target_os = "windows")]
pub async fn get_invalid_windows_mesa_loader_file(
  app: &AppHandle,
  source: SourceType,
  library_path: &Path,
  game_config: &GameConfig,
  check_hash: bool,
) -> SJMCLResult<Vec<PTaskParam>> {
  if mesa_driver_name(
    &game_config.advanced.graphics.api,
    &game_config.advanced.graphics.renderer,
  )
  .is_none()
  {
    return Ok(Vec::new());
  }

  let Some(lib) = get_windows_mesa_loader_library(app)? else {
    return Ok(Vec::new());
  };
  let Some(artifact) = lib.downloads.and_then(|downloads| downloads.artifact) else {
    return Ok(Vec::new());
  };

  let file_path = library_path.join(&artifact.path);
  let exists = fs::try_exists(&file_path).await?;
  if exists && (!check_hash || validate_sha1(file_path.clone(), artifact.sha1.clone()).is_ok()) {
    return Ok(Vec::new());
  }

  let src = convert_url_to_target_source(
    &url::Url::parse(&artifact.url)?,
    &[ResourceType::Libraries],
    &source,
  )?;
  Ok(vec![PTaskParam::Download(DownloadParam {
    src,
    dest: file_path,
    filename: None,
    sha1: Some(artifact.sha1),
  })])
}

pub struct LibraryParts {
  pub path: String,
  pub pack_name: String,
  pub pack_version: String,
  pub classifier: Option<String>,
  pub extension: String,
}

pub fn parse_library_name(name: &str, native: Option<String>) -> SJMCLResult<LibraryParts> {
  let parts: Vec<&str> = name.split('@').collect();
  let file_ext = if parts.len() > 1 {
    parts[1].to_string()
  } else {
    "jar".to_string()
  };
  let mut name_split: Vec<String> = parts[0].split(':').map(|s| s.to_string()).collect();
  if name_split.len() < 3 {
    Err(InstanceError::InvalidSourcePath.into())
  } else {
    if let Some(native) = native {
      name_split.push(native);
    }
    let pack_name = name_split[1].clone();
    let pack_version = name_split[2].clone();
    let classifier = if name_split.len() > 3 {
      Some(name_split[3].clone())
    } else {
      None
    };
    let path = name_split[0].replace('.', "/");
    Ok(LibraryParts {
      path,
      pack_name,
      pack_version,
      classifier,
      extension: file_ext,
    })
  }
}

#[derive(Debug, Hash, Eq, PartialEq)]
struct LibraryKey {
  path: String,
  pack_name: String,
  classifier: Option<String>,
  extension: String,
}

// merge two vectors of libraries, remove duplicates by name, keep the one with the highest version. also remove libraries with invalid names
pub fn merge_library_lists(
  libraries_a: &[LibrariesValue],
  libraries_b: &[LibrariesValue],
) -> Vec<LibrariesValue> {
  let mut library_indices: HashMap<LibraryKey, usize> = HashMap::new();
  let mut libraries: Vec<LibrariesValue> = Vec::new();

  for library in libraries_a.iter().chain(libraries_b.iter()) {
    if let Ok(library_parts) = parse_library_name(&library.name, None) {
      let key = LibraryKey {
        path: library_parts.path,
        pack_name: library_parts.pack_name,
        classifier: library_parts.classifier,
        extension: library_parts.extension,
      };

      let new_version = library_parts.pack_version;

      if let Some(&index) = library_indices.get(&key) {
        let existing_version = parse_library_name(&libraries[index].name, None)
          .map(|parts| parts.pack_version)
          .unwrap_or("0.1.0".to_string());

        if parse_sem_version(&new_version) > parse_sem_version(&existing_version) {
          libraries[index] = library.clone();
        }
      } else {
        library_indices.insert(key, libraries.len());
        libraries.push(library.clone());
      }
    }
  }

  libraries
}

/// Insert a library into the library list of a client json, following Prism's
/// `LaunchProfile::applyLibrary`: a library is looked up by its artifact (group, name,
/// classifier and extension, without the version), and it replaces the applied library only
/// when it declares a higher version. A replacement keeps the position of the applied library,
/// so that the classpath order stays stable.
pub fn apply_library(libraries: &mut Vec<LibrariesValue>, library: LibrariesValue) {
  let Ok(new_parts) = parse_library_name(&library.name, None) else {
    if !libraries.iter().any(|item| item.name == library.name) {
      libraries.push(library);
    }
    return;
  };

  let applied_index = libraries.iter().position(|item| {
    parse_library_name(&item.name, None).is_ok_and(|parts| {
      parts.path == new_parts.path
        && parts.pack_name == new_parts.pack_name
        && parts.classifier == new_parts.classifier
        && parts.extension == new_parts.extension
    })
  });

  match applied_index {
    Some(index) => {
      let applied_version = parse_library_name(&libraries[index].name, None)
        .map(|parts| parts.pack_version)
        .unwrap_or_default();
      if compare_library_versions(&new_parts.pack_version, &applied_version).is_gt() {
        libraries[index] = library;
      }
    }
    None => libraries.push(library),
  }
}

/// Insert a library of the base game patch into the library list of a client json. Unlike a
/// component library, it never replaces an applied library of the same artifact: the component
/// definition wins, e.g. the patched log4j build a legacy modpack ships.
pub fn apply_base_library(libraries: &mut Vec<LibrariesValue>, library: LibrariesValue) {
  let Ok(parts) = parse_library_name(&library.name, None) else {
    if !libraries.iter().any(|item| item.name == library.name) {
      libraries.push(library);
    }
    return;
  };

  let applied = libraries.iter().any(|item| {
    parse_library_name(&item.name, None).is_ok_and(|applied| {
      applied.path == parts.path
        && applied.pack_name == parts.pack_name
        && applied.classifier == parts.classifier
        && applied.extension == parts.extension
    })
  });
  if !applied {
    libraries.push(library);
  }
}

/// Compare two library versions section by section, like Prism's `Version` does: a section of
/// digits is compared numerically, any other section byte by byte. A version that already ended
/// ranks above a pre-release section (`-...`) and below any other section.
fn compare_library_versions(a: &str, b: &str) -> Ordering {
  let a_sections = version_sections(a);
  let b_sections = version_sections(b);

  for index in 0..a_sections.len().max(b_sections.len()) {
    let ordering = match (a_sections.get(index), b_sections.get(index)) {
      (None, None) => Ordering::Equal,
      // The left version ended here.
      (None, Some(section)) => {
        if section.starts_with('-') {
          Ordering::Greater
        } else {
          Ordering::Less
        }
      }
      // The right version ended here.
      (Some(section), None) => {
        if section.starts_with('-') {
          Ordering::Less
        } else {
          Ordering::Greater
        }
      }
      (Some(left), Some(right)) => compare_version_sections(left, right),
    };
    if ordering != Ordering::Equal {
      return ordering;
    }
  }

  Ordering::Equal
}

fn compare_version_sections(left: &str, right: &str) -> Ordering {
  if left.chars().all(|character| character.is_ascii_digit())
    && right.chars().all(|character| character.is_ascii_digit())
  {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    return left.len().cmp(&right.len()).then_with(|| left.cmp(right));
  }

  left
    .chars()
    .zip(right.chars())
    .map(|(a, b)| a.cmp(&b))
    .find(|ordering| *ordering != Ordering::Equal)
    .unwrap_or_else(|| left.len().cmp(&right.len()))
}

/// Split a version into the compared sections, e.g. `2.9.4-nightly-20150209` is
/// `["2", "9", "4", "-nightly", "-20150209"]`. Everything after `+` is ignored, like Prism does.
fn version_sections(version: &str) -> Vec<String> {
  let mut sections = Vec::new();
  let mut current = String::new();
  let mut is_digits = None;

  for character in version.chars() {
    if character == '+' {
      break;
    }
    if character == '.' || character == '-' {
      if !current.is_empty() {
        sections.push(std::mem::take(&mut current));
      }
      // A dash marks the following section as a pre-release.
      if character == '-' {
        current.push(character);
      }
      is_digits = None;
      continue;
    }
    let digits = character.is_ascii_digit();
    if is_digits.is_some_and(|is_digits| is_digits != digits) {
      sections.push(std::mem::take(&mut current));
    }
    is_digits = Some(digits);
    current.push(character);
  }
  if !current.is_empty() {
    sections.push(current);
  }

  sections
}

fn parse_sem_version(version: &str) -> Version {
  Version::parse(version).unwrap_or({
    let mut parts = version.split('.').collect::<Vec<_>>();
    while parts.len() < 3 {
      parts.push("0");
    }
    Version::parse(&parts[..3].join(".")).unwrap_or(Version::new(0, 1, 0))
  })
}

pub fn convert_library_name_to_path(name: &str, native: Option<String>) -> SJMCLResult<String> {
  let LibraryParts {
    path,
    pack_name,
    pack_version,
    classifier,
    extension: file_ext,
  } = parse_library_name(name, native)?;

  let file_name = [
    pack_name.clone(),
    pack_version.clone(),
    classifier.unwrap_or_default(),
  ]
  .iter()
  .filter(|s| !s.is_empty())
  .map(|s| s.as_str())
  .collect::<Vec<&str>>()
  .join("-")
    + "."
    + &file_ext;
  Ok(format!("{path}/{pack_name}/{pack_version}/{file_name}"))
}

pub fn get_nonnative_library_paths(
  client_info: &McClientInfo,
  library_path: &Path,
  mod_loader_type: ModLoaderType,
) -> SJMCLResult<Vec<PathBuf>> {
  let mut libraries = Vec::new();
  let feature = FeaturesInfo::default();
  for library in &client_info.libraries {
    if !library.is_allowed(&feature).unwrap_or(false) {
      continue;
    }
    if library.natives.is_some() {
      continue;
    }

    if mod_loader_type == ModLoaderType::Cleanroom {
      // Cleanroom replaces legacy LWJGL 2 libraries, whose versions vary by platform.
      if library.name.starts_with("org.lwjgl.lwjgl:lwjgl:")
        || library.name.starts_with("org.lwjgl.lwjgl:lwjgl_util:")
        || matches!(
          library.name.as_str(),
          "net.java.dev.jna:platform:3.4.0" | "com.ibm.icu:icu4j-core-mojang:51.2"
        )
      {
        continue;
      }
    }

    libraries.push(library.clone());
  }

  libraries = merge_library_lists(&libraries, &[]); // remove duplicates to prevent launch errors
  let mut result = Vec::new();
  for library in libraries {
    let path = library
      .downloads
      .as_ref()
      .and_then(|downloads| downloads.artifact.as_ref())
      .map(|artifact| artifact.path.as_str())
      .filter(|path| !path.is_empty());
    result.push(library_path.join(match path {
      Some(path) => path.to_string(),
      None => convert_library_name_to_path(&library.name, None)?,
    }));
  }
  Ok(result)
}

pub fn get_native_library_paths(
  client_info: &McClientInfo,
  library_path: &Path,
  #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] use_native_glfw: bool,
  #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] use_native_openal: bool,
) -> SJMCLResult<Vec<PathBuf>> {
  let mut result = Vec::new();
  let feature = FeaturesInfo::default();
  for library in &client_info.libraries {
    if !library.is_allowed(&feature).unwrap_or(false) {
      continue;
    }
    if let Some(natives) = &library.natives {
      if let Some(native) = get_natives_string(natives) {
        let path = convert_library_name_to_path(&library.name, Some(native))?;
        #[cfg(target_os = "linux")]
        {
          if use_native_glfw && library.name.to_lowercase().contains("glfw") {
            continue;
          }
          if use_native_openal && library.name.to_lowercase().contains("openal") {
            continue;
          }
        }
        result.push(library_path.join(path));
      } else {
        println!("natives is None");
      }
    }
  }
  Ok(result)
}

fn get_native_library_extract_dir(client_info: &McClientInfo, natives_dir: &Path) -> PathBuf {
  const JAVA_LIBRARY_PATH_PREFIX: &str = "-Djava.library.path=";
  const NATIVES_SUBDIR_PREFIX: &str = "${natives_directory}/";

  let Some(arguments) = &client_info.arguments else {
    return natives_dir.to_path_buf();
  };

  for argument in &arguments.jvm {
    if !argument.rules.is_empty() {
      continue;
    }

    for value in &argument.value {
      let Some(path) = value.strip_prefix(JAVA_LIBRARY_PATH_PREFIX) else {
        continue;
      };

      // Minecraft versions before 26.2-snapshot-1 use ${natives_directory} directly.
      if path == "${natives_directory}" {
        return natives_dir.to_path_buf();
      }

      // Minecraft 26.2-snapshot-1+ may use a subdirectory such as ${natives_directory}/java.
      let Some(relative_path) = path.strip_prefix(NATIVES_SUBDIR_PREFIX) else {
        return natives_dir.to_path_buf();
      };
      let relative_path = Path::new(relative_path);

      if relative_path.as_os_str().is_empty()
        || !relative_path
          .components()
          .all(|component| matches!(component, Component::Normal(_)))
      {
        return natives_dir.to_path_buf();
      }

      return natives_dir.join(relative_path);
    }
  }

  natives_dir.to_path_buf()
}

pub async fn extract_native_libraries(
  client_info: &McClientInfo,
  library_path: &Path,
  natives_dir: &Path,
  use_native_glfw: bool,
  use_native_openal: bool,
) -> SJMCLResult<()> {
  #[cfg(target_os = "linux")]
  if (use_native_glfw || use_native_openal) && natives_dir.exists() {
    fs::remove_dir_all(natives_dir).await?;
  }

  let extract_dir = get_native_library_extract_dir(client_info, natives_dir);
  fs::create_dir_all(&extract_dir).await?;

  let native_libraries = get_native_library_paths(
    client_info,
    library_path,
    use_native_glfw,
    use_native_openal,
  )?;
  let tasks: Vec<tokio::task::JoinHandle<SJMCLResult<()>>> = native_libraries
    .into_iter()
    .map(|library_path| {
      let extract_dir = extract_dir.clone();
      tokio::spawn(async move {
        let file = Cursor::new(fs::read(library_path).await?);
        let mut jar = ZipArchive::new(file)?;
        jar.extract(&extract_dir)?;
        Ok(())
      })
    })
    .collect();

  let results = futures::future::join_all(tasks).await;

  for result in results {
    if let Err(e) = result {
      println!("Error handling artifact: {:?}", e);
      return Err(sjmcl_types::error::SJMCLError::from(e));
    }
  }

  Ok(())
}

pub async fn get_invalid_assets(
  app: &AppHandle,
  client_info: &McClientInfo,
  source: SourceType,
  asset_path: &Path,
  check_hash: bool,
) -> SJMCLResult<Vec<PTaskParam>> {
  let assets_download_api = get_download_api(source, ResourceType::Assets)?;

  let asset_index_path = asset_path.join(format!("indexes/{}.json", client_info.asset_index.id));
  let asset_index = load_asset_index(app, &asset_index_path, &client_info.asset_index.url).await?;

  let futs = asset_index.objects.into_values().map(|item| {
    let assets_download_api = assets_download_api.clone();
    let base_path = asset_path.to_path_buf();

    async move {
      let path_in_repo = format!("{}/{}", &item.hash[..2], item.hash);
      let dest = base_path.join(format!("objects/{}", path_in_repo));
      let exists = fs::try_exists(&dest).await?;

      if exists && (!check_hash || validate_sha1(dest.clone(), item.hash.clone()).is_ok()) {
        Ok::<Option<PTaskParam>, sjmcl_types::error::SJMCLError>(None)
      } else {
        let src = assets_download_api
          .join(&path_in_repo)
          .map_err(sjmcl_types::error::SJMCLError::from)?;
        Ok(Some(PTaskParam::Download(DownloadParam {
          src,
          dest,
          filename: None,
          sha1: Some(item.hash.clone()),
        })))
      }
    }
  });

  let results: Vec<SJMCLResult<Option<PTaskParam>>> = join_all(futs).await;

  let mut params = Vec::new();
  for r in results {
    if let Some(p) = r? {
      params.push(p);
    }
  }
  Ok(params)
}

pub async fn prepare_legacy_assets(
  root_dir: &Path,
  assets_dir: &Path,
  assets_index_name: &str,
) -> SJMCLResult<()> {
  let target_roots = match assets_index_name {
    "legacy" => vec![assets_dir.join("virtual/legacy")],
    "pre-1.6" => vec![
      assets_dir.join("virtual/legacy"),
      root_dir.join("resources"),
    ],
    _ => return Ok(()),
  };

  let objects_dir = assets_dir.join("objects");
  let asset_index =
    load_json_async::<AssetIndex>(&assets_dir.join(format!("indexes/{}.json", assets_index_name)))
      .await?;

  stream::iter(asset_index.objects)
    .map(Ok::<_, SJMCLError>)
    .try_for_each_concurrent(None, move |(name, item)| {
      let origin = objects_dir.join(format!("{}/{}", &item.hash[..2], item.hash));
      let targets = target_roots
        .iter()
        .map(|target_root| target_root.join(&name))
        .collect::<Vec<_>>();

      async move {
        if !fs::try_exists(&origin).await? {
          return Ok::<(), SJMCLError>(());
        }

        for target in targets {
          if fs::try_exists(&target).await? {
            continue;
          }
          if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
          }
          fs::copy(&origin, &target).await?;
        }
        Ok(())
      }
    })
    .await?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn library(name: &str) -> LibrariesValue {
    LibrariesValue {
      name: name.to_string(),
      ..Default::default()
    }
  }

  #[test]
  fn merge_library_lists_preserves_classpath_order() {
    let merged = merge_library_lists(
      &[
        library("com.github.GTNewHorizons:lwjgl3ify:2.1.16:forgePatches"),
        library("commons-io:commons-io:2.4"),
      ],
      &[library("commons-io:commons-io:2.5")],
    );
    assert_eq!(
      merged
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>(),
      [
        "com.github.GTNewHorizons:lwjgl3ify:2.1.16:forgePatches",
        "commons-io:commons-io:2.5"
      ]
    );
  }

  #[test]
  fn apply_library_replaces_applied_artifacts_only_with_higher_versions() {
    let mut libraries = vec![
      library("org.lwjgl.lwjgl:lwjgl:2.9.1"),
      library("org.lwjgl.lwjgl:lwjgl_util:2.9.1"),
    ];

    // A component of an old modpack may declare a higher version of a library that the game
    // itself already applied, e.g. its LWJGL version. The position of the library is kept.
    apply_library(
      &mut libraries,
      library("org.lwjgl.lwjgl:lwjgl:2.9.4-nightly-20150209"),
    );
    assert_eq!(
      libraries
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>(),
      [
        "org.lwjgl.lwjgl:lwjgl:2.9.4-nightly-20150209",
        "org.lwjgl.lwjgl:lwjgl_util:2.9.1"
      ]
    );

    // A lower version never replaces an applied library.
    apply_library(&mut libraries, library("org.lwjgl.lwjgl:lwjgl:2.9.1"));
    assert_eq!(
      libraries[0].name,
      "org.lwjgl.lwjgl:lwjgl:2.9.4-nightly-20150209"
    );

    // A different classifier is a different artifact and is applied next to it.
    apply_library(
      &mut libraries,
      library("org.lwjgl.lwjgl:lwjgl:2.9.1:natives-windows"),
    );
    assert_eq!(libraries.len(), 3);
  }

  #[test]
  fn maven_files_are_downloaded_but_stay_off_the_classpath() {
    let maven_file: LibrariesValue = serde_json::from_str(
      r#"{
        "name": "net.minecraftforge:forge:1.20.1-47.2.0:installer",
        "downloads": {
          "artifact": {
            "path": "net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar",
            "url": "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar",
            "sha1": "ded43dd1",
            "size": 5973950
          }
        }
      }"#,
    )
    .unwrap();
    let client_info = McClientInfo {
      libraries: vec![LibrariesValue {
        name: "org.lwjgl.lwjgl:lwjgl:2.9.1".to_string(),
        ..Default::default()
      }],
      maven_files: vec![maven_file],
      ..Default::default()
    };

    assert_eq!(get_maven_file_artifacts(&client_info).len(), 1);
    assert!(
      get_nonnative_library_artifacts(&client_info)
        .iter()
        .all(|artifact| !artifact.path.contains("installer"))
    );
  }
}
