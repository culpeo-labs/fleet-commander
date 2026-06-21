//! Resolving the container image (pull/build + features).

use std::collections::HashMap;
use std::path::Path;

use devcontainer_lib::devcontainer::config::DevcontainerConfig;
use devcontainer_lib::devcontainer::features::{
    download_features, generate_feature_dockerfile_with_opts, order_features, resolve_features,
    stage_feature_context,
};
use devcontainer_lib::runtime::{self, ContainerRuntime};
use devcontainer_lib::util::container_name;
use tracing::{debug, info, warn};

use super::ContainerError;

/// Resolve the container image — pull/build base, then layer features if present.
pub(super) async fn resolve_image(
    rt: &dyn ContainerRuntime,
    workspace: &Path,
    config: &DevcontainerConfig,
    config_path: &Path,
    on_progress: &impl Fn(&str),
) -> Result<String, ContainerError> {
    let folder_image = container_name(workspace);
    let features = resolve_features(config).unwrap_or_default();
    let has_features = !features.is_empty();
    let devcontainer_dir = config_path.parent().map(|p| p.to_path_buf());
    debug!(
        has_features,
        feature_count = features.len(),
        "Resolving image"
    );

    // 1. Pull or build the base image.
    let base_image = if let Some(ref image) = config.image {
        info!(image = %image, "Resolving base image");
        let cached = rt.image_exists(image).await.unwrap_or(false);
        if cached {
            on_progress("Base image cached locally ✓");
            info!(image = %image, "Base image found locally, skipping pull");
        } else {
            on_progress(&format!("Pulling image {image}…"));
            info!(image = %image, "Pulling base image");
            rt.pull_image(image)
                .await
                .map_err(|e| ContainerError::Start(format!("Failed to pull image: {e}")))?;
            on_progress("Image pull complete ✓");
            info!(image = %image, "Base image pull complete");
        }
        image.clone()
    } else if let Some(ref build) = config.build {
        let context_dir = config_path
            .parent()
            .unwrap()
            .join(build.context.as_deref().unwrap_or("."));
        let dockerfile_path = config_path.parent().unwrap().join(&build.dockerfile);
        info!(dockerfile = %dockerfile_path.display(), context = %context_dir.display(), "Building base image");
        on_progress("Building image from Dockerfile…");
        let dockerfile_content = std::fs::read_to_string(&dockerfile_path)
            .map_err(|e| ContainerError::Parse(format!("Failed to read Dockerfile: {e}")))?;
        let base_tag = if has_features {
            folder_image.clone()
        } else {
            format!("{folder_image}-base")
        };
        rt.build_image(
            &dockerfile_content,
            &context_dir,
            &base_tag,
            &HashMap::new(),
            false,
            false,
        )
        .await
        .map_err(|e| ContainerError::Start(format!("Image build failed: {e}")))?;
        base_tag
    } else {
        return Err(ContainerError::Parse(
            "devcontainer.json must specify 'image' or 'build.dockerfile'".into(),
        ));
    };

    if !has_features {
        info!(image = %base_image, "No features, using base image directly");
        return Ok(base_image);
    }

    // 2. Download and stage features.
    info!(count = features.len(), "Downloading features");
    on_progress(&format!("Downloading {} feature(s)…", features.len()));
    let mut features = features;
    download_features(&mut features, devcontainer_dir.as_deref())
        .await
        .map_err(|e| ContainerError::Start(format!("Failed to download features: {e}")))?;

    let ordered = order_features(&features);
    let staging_dir = stage_feature_context(&ordered)
        .map_err(|e| ContainerError::Start(format!("Failed to stage features: {e}")))?;

    // 3. Generate and build the feature layer.
    let feature_user = runtime::resolve_remote_user(rt, &base_image, config.remote_user.as_deref())
        .await
        .ok()
        .flatten();
    let dockerfile = generate_feature_dockerfile_with_opts(
        &base_image,
        &ordered,
        feature_user.as_deref(),
        config,
    );
    let final_tag = format!("{folder_image}-features");
    info!(tag = %final_tag, "Building feature layer image");
    debug!(dockerfile = %dockerfile, "Feature layer Dockerfile");
    on_progress("Building feature layer…");
    let result = rt
        .build_image(
            &dockerfile,
            &staging_dir,
            &final_tag,
            &HashMap::new(),
            false,
            false,
        )
        .await;
    let _ = std::fs::remove_dir_all(&staging_dir);

    match result {
        Ok(()) => {
            info!(tag = %final_tag, "Feature image ready");
            Ok(final_tag)
        }
        Err(e) => {
            warn!(error = %e, "Feature layer build failed, falling back to base image");
            on_progress("⚠ Feature build failed, using base image…");
            Ok(base_image)
        }
    }
}

/// Resolve the effective remote user from config or image metadata.
pub(super) async fn resolve_remote_user(
    rt: &dyn ContainerRuntime,
    image: &str,
    config: &DevcontainerConfig,
) -> Result<Option<String>, ContainerError> {
    runtime::resolve_remote_user(rt, image, config.remote_user.as_deref())
        .await
        .map_err(|e| ContainerError::Start(e.to_string()))
}
