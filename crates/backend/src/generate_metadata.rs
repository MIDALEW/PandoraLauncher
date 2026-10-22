//! Generates provider metadata for content files that were installed manually.
//!
//! This reads the metadata that the mod metadata manager already extracted from the
//! archive (`fabric.mod.json`, `quilt.mod.json`, `META-INF/mods.toml`, `mcmod.info`, ...),
//! hashes the file the way each provider expects, and asks the provider APIs which project
//! the file belongs to. The resulting `ContentSource` is persisted in the existing content
//! sources store, so update checks and the change-version dialog can use it later.

use std::{path::Path, sync::Arc};

use bridge::{
    instance::{ContentFolder, InstanceContentID, InstanceID},
    modal_action::{ModalAction, ProgressTrackerFinishType},
};
use schema::{
    content::ContentSource,
    curseforge::{CurseforgeFingerprintRequest, CurseforgeGetModsRequest},
    modrinth::{ModrinthProjectsRequest, ModrinthVersionsFromHashesRequest},
};
use sha2::{Digest, Sha512};

use crate::{
    BackendState,
    metadata::items::{
        CurseforgeFingerprintMetadataItem, CurseforgeGetModsMetadataItem, ModrinthProjectsMetadataItem,
        ModrinthVersionsFromHashesMetadataItem,
    },
};

struct Target {
    hash: [u8; 20],
    path: Arc<Path>,
    name: Arc<str>,
}

struct FileHashes {
    sha512: String,
    fingerprint: u32,
}

/// Computes the hashes used by Modrinth (`sha512`) and CurseForge (Murmur2 fingerprint).
fn compute_file_hashes(path: &Path) -> std::io::Result<FileHashes> {
    let mut data = std::fs::read(path)?;
    let sha512 = hex::encode(Sha512::digest(&data));

    // CurseForge computes its fingerprint over the file contents with whitespace removed.
    data.retain(|byte| !matches!(byte, b'\t' | b'\n' | b'\r' | b' '));

    Ok(FileHashes {
        sha512,
        fingerprint: murmur2_32(&data),
    })
}

/// Computes a CurseForge fingerprint for a file on disk.
pub(crate) fn compute_curseforge_fingerprint(path: &Path) -> std::io::Result<u32> {
    let mut data = std::fs::read(path)?;
    data.retain(|byte| !matches!(byte, b'\t' | b'\n' | b'\r' | b' '));
    Ok(murmur2_32(&data))
}

fn murmur2_32(data: &[u8]) -> u32 {
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;

    let len = data.len() as u32;
    // CurseForge uses MurmurHash2 with seed 1, not the more common seed 0.
    let mut h = len ^ 1;

    let mut i = 0;
    while i + 4 <= data.len() {
        let mut k = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);

        h = h.wrapping_mul(M);
        h ^= k;

        i += 4;
    }

    match data.len() & 3 {
        3 => {
            h ^= (data[i + 2] as u32) << 16;
            h ^= (data[i + 1] as u32) << 8;
            h ^= data[i] as u32;
            h = h.wrapping_mul(M);
        },
        2 => {
            h ^= (data[i + 1] as u32) << 8;
            h ^= data[i] as u32;
            h = h.wrapping_mul(M);
        },
        1 => {
            h ^= data[i] as u32;
            h = h.wrapping_mul(M);
        },
        _ => {},
    }

    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;

    h
}

async fn detect_modrinth(backend: &BackendState, sha512: &str) -> Option<(Arc<str>, Option<Arc<str>>)> {
    let request = ModrinthVersionsFromHashesRequest {
        hashes: Arc::from([Arc::<str>::from(sha512)]),
        algorithm: "sha512".into(),
    };

    let response = backend.meta.fetch(ModrinthVersionsFromHashesMetadataItem(&request)).await.ok()?;

    let version = response.0.get(sha512)?.as_ref()?;
    let project_id = Arc::clone(&version.project_id);

    let projects_request = ModrinthProjectsRequest {
        ids: Arc::from([Arc::clone(&project_id)]),
    };
    let slug = backend
        .meta
        .fetch(ModrinthProjectsMetadataItem(&projects_request))
        .await
        .ok()
        .and_then(|projects| {
            projects
                .0
                .iter()
                .find(|project| project.id == project_id)
                .and_then(|project| project.slug.clone())
        });

    Some((project_id, slug))
}

async fn detect_curseforge(backend: &BackendState, fingerprint: u32) -> Option<(u32, Option<Arc<str>>)> {
    let request = CurseforgeFingerprintRequest {
        fingerprints: vec![fingerprint],
    };

    let response = backend.meta.fetch(CurseforgeFingerprintMetadataItem(&request)).await.ok()?;

    let mod_id = response
        .data
        .exact_matches
        .iter()
        .find(|matched| matched.file.file_fingerprint == fingerprint)?
        .file
        .mod_id;

    let mods_request = CurseforgeGetModsRequest { mod_ids: vec![mod_id] };
    let slug = backend
        .meta
        .fetch(CurseforgeGetModsMetadataItem(&mods_request))
        .await
        .ok()
        .and_then(|mods| {
            mods.data
                .iter()
                .find(|project| project.id == mod_id)
                .and_then(|project| project.slug.clone())
        });

    Some((mod_id, slug))
}

/// Detects the Modrinth/CurseForge project for each given content entry and persists it.
pub async fn generate_metadata(
    backend: Arc<BackendState>,
    instance_id: InstanceID,
    content_ids: Vec<InstanceContentID>,
    modal_action: &ModalAction,
) {
    let (targets, content_folder) = {
        let guard = backend.instance_state.read();
        let Some(instance) = guard.instances.get(instance_id) else {
            backend.send.send_error("Unable to generate metadata, unknown instance");
            return;
        };

        let mut targets = Vec::with_capacity(content_ids.len());
        let mut folder = None;
        for content_id in content_ids {
            let Some((summary, content_folder)) = instance.try_get_content(content_id) else {
                continue;
            };

            // Only fill in missing metadata. Existing provider links are authoritative.
            if matches!(
                summary.content_source,
                ContentSource::ModrinthProject { .. } | ContentSource::CurseforgeProject { .. }
            ) {
                continue;
            }

            folder.get_or_insert(content_folder);
            targets.push(Target {
                hash: summary.content_summary.hash,
                path: Arc::clone(&summary.path),
                name: summary
                    .content_summary
                    .name
                    .clone()
                    .or_else(|| summary.content_summary.id.clone())
                    .unwrap_or_else(|| summary.filename.clone()),
            });
        }

        (targets, folder.unwrap_or(ContentFolder::Mods))
    };

    if targets.is_empty() {
        return;
    }

    let tracker = modal_action.push_tracker("Generating metadata".into());
    tracker.set_total(targets.len());

    let mut modrinth_count = 0_usize;
    let mut curseforge_count = 0_usize;
    let mut not_found = Vec::new();
    let mut failed = Vec::new();

    for target in targets {
        tracker.add_count(1);

        let path = Arc::clone(&target.path);
        let hashes = match tokio::task::spawn_blocking(move || compute_file_hashes(&path)).await {
            Ok(Ok(hashes)) => hashes,
            Ok(Err(error)) => {
                log::warn!("Unable to hash {:?}: {}", target.path, error);
                failed.push(target.name);
                continue;
            },
            Err(error) => {
                log::warn!("Unable to hash {:?}: {}", target.path, error);
                failed.push(target.name);
                continue;
            },
        };

        // Modrinth takes priority; CurseForge is only queried when Modrinth has no match.
        if let Some((project_id, slug)) = detect_modrinth(&backend, &hashes.sha512).await {
            log::info!(
                "Detected Modrinth project for {}: {} ({})",
                target.name,
                project_id,
                slug.as_deref().unwrap_or("unknown slug")
            );
            backend
                .mod_metadata_manager
                .set_content_source(target.hash, ContentSource::ModrinthProject { project_id });
            modrinth_count += 1;
            continue;
        }

        if let Some((project_id, slug)) = detect_curseforge(&backend, hashes.fingerprint).await {
            log::info!(
                "Detected CurseForge project for {}: {} ({})",
                target.name,
                project_id,
                slug.as_deref().unwrap_or("unknown slug")
            );
            backend
                .mod_metadata_manager
                .set_content_source(target.hash, ContentSource::CurseforgeProject { project_id });
            curseforge_count += 1;
            continue;
        }

        not_found.push(target.name);
    }

    backend.mod_metadata_manager.write_changes();

    if let Some(instance) = backend.instance_state.write().instances.get_mut(instance_id) {
        // Content sources are read while content is (re)loaded, so force a full reload.
        instance.mark_content_dirty(&backend, content_folder, crate::fs::FolderChanges::all_dirty(), true);
    }

    tracker.set_finished(ProgressTrackerFinishType::Normal);

    let total_detected = modrinth_count + curseforge_count;
    if total_detected > 0 {
        backend.send.send_success(format!(
            "Detected metadata for {} file(s): {} from Modrinth, {} from CurseForge",
            total_detected, modrinth_count, curseforge_count
        ));
    }

    if !not_found.is_empty() {
        backend.send.send_warning(format!(
            "No project found for {} file(s): {}",
            not_found.len(),
            not_found.join(", ")
        ));
    }

    if !failed.is_empty() {
        backend
            .send
            .send_error(format!("Unable to read {} file(s): {}", failed.len(), failed.join(", ")));
    }

    backend.send.send(bridge::message::MessageToFrontend::Refresh);
}

#[cfg(test)]
mod tests {
    use super::murmur2_32;

    #[test]
    fn murmur2_matches_curseforge_fingerprints() {
        // Reference values produced by the CurseForge fingerprint algorithm
        // (MurmurHash2, seed 1) for the same inputs.
        assert_eq!(murmur2_32(b""), 1_540_447_798);
        assert_eq!(murmur2_32(b"hello"), 2_788_266_382);
        assert_eq!(murmur2_32(b"The quick brown fox jumps over the lazy dog"), 504_383_975);
    }
}
