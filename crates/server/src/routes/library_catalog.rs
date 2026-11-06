//! Library import/removal and device-catalog HTTP handlers.

use std::fs;

use axum::{
    extract::{Path as AxumPath, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::{emit_mutation, with_project, ProjectName, RunResponse};
use crate::error::ApiError;
use crate::events::{topic, MutationDetail};
use crate::state::AppState;

/// GET /api/library — every library in the server's registry, each
/// decorated with its imported state in the addressed project (when
/// one is open; browse works without).
pub async fn list_libraries(
    State(state): State<AppState>,
    project: ProjectName,
) -> Json<Vec<crate::library::LibrarySummary>> {
    let registry = crate::library::scan(state.library_dir.as_deref());
    let mut out: Vec<_> = registry.iter().map(|l| l.to_summary()).collect();
    // No open project (agent browsing a fresh server) → plain listing.
    let _ = with_project(&state, &project, |store| {
        let versions = store.read_libraries()?;
        let slugs = store.list_pou_paths()?;
        for summary in &mut out {
            summary.imported_version = versions.get(&summary.name).cloned();
            let prefix = format!("{}/{}/", project::LIBRARY_SLUG_PREFIX, summary.name);
            summary.imported_files = slugs
                .iter()
                .filter_map(|s| s.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(str::to_string)
                .collect();
        }
        Ok(())
    });
    Json(out)
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct ImportLibraryRequest {
    /// Registry library name, e.g. `process-control`.
    pub library: String,
    /// Bare block file names to import (`fb_pid.st`). Empty / omitted
    /// imports every block in the library. Re-importing overwrites —
    /// that's the update path.
    #[serde(default)]
    pub blocks: Vec<String>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ImportLibraryResponse {
    pub library: String,
    pub version: String,
    /// Block files written under `pous/lib/<library>/`.
    pub imported: Vec<String>,
}

/// POST /api/library/import — vendor registry blocks into the project.
pub async fn import_library(
    State(state): State<AppState>,
    project: ProjectName,
    Json(req): Json<ImportLibraryRequest>,
) -> Result<Json<ImportLibraryResponse>, ApiError> {
    let registry = crate::library::scan(state.library_dir.as_deref());
    let lib = registry
        .into_iter()
        .find(|l| l.name == req.library)
        .ok_or_else(|| ApiError::NotFound(format!("library '{}' not in registry", req.library)))?;

    let chosen: Vec<&crate::library::RegistryBlockFile> = if req.blocks.is_empty() {
        lib.blocks.iter().collect()
    } else {
        req.blocks
            .iter()
            .map(|want| {
                lib.blocks.iter().find(|b| &b.file == want).ok_or_else(|| {
                    ApiError::BadRequest(format!(
                        "library '{}' has no block file '{want}'",
                        lib.name
                    ))
                })
            })
            .collect::<Result<_, _>>()?
    };

    // Read every source up-front so an unreadable registry file fails
    // the request before the project is half-written.
    let mut files = Vec::with_capacity(chosen.len());
    for block in &chosen {
        let content = fs::read_to_string(&block.path).map_err(|e| {
            ApiError::Internal(format!("reading registry block '{}': {e}", block.file))
        })?;
        files.push((block.file.clone(), content));
    }

    let imported = with_project(&state, &project, |store| {
        for (file, content) in &files {
            store.write_library_file(&lib.name, file, content)?;
        }
        store.set_library_entry(&lib.name, &lib.version)?;
        Ok(files.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>())
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::LibraryImported {
            name: lib.name.clone(),
        },
    );
    Ok(Json(ImportLibraryResponse {
        library: lib.name,
        version: lib.version,
        imported,
    }))
}

/// GET /api/device-catalog — validated device templates from
/// `<library-dir>/devices/` (see library/devices/README.md).
pub async fn list_device_catalog(
    State(state): State<AppState>,
) -> Json<Vec<crate::catalog::CatalogEntry>> {
    Json(crate::catalog::scan(state.library_dir.as_deref()))
}

#[derive(Debug, Deserialize)]
pub struct CatalogMatchQuery {
    pub vendor_id: u32,
    pub product_id: u32,
}

/// GET /api/device-catalog/match?vendor_id=&product_id= — resolve a
/// discovered slave's identity to a catalog template, so the IDE /
/// agent can pre-fill a device from a bus scan instead of hand-typing
/// PDI offsets. 404 when the identity isn't in the catalog.
pub async fn match_device_catalog(
    State(state): State<AppState>,
    Query(q): Query<CatalogMatchQuery>,
) -> Result<Json<crate::catalog::CatalogEntry>, ApiError> {
    let entries = crate::catalog::scan(state.library_dir.as_deref());
    crate::catalog::match_identity(&entries, q.vendor_id, q.product_id)
        .cloned()
        .map(Json)
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "no catalog entry for vendor_id={} product_id={}",
                q.vendor_id, q.product_id
            ))
        })
}

/// DELETE /api/library/{name} — drop `pous/lib/<name>/` and the
/// project.toml entry. Idempotent on the directory.
pub async fn remove_library(
    State(state): State<AppState>,
    project: ProjectName,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<RunResponse>, ApiError> {
    with_project(&state, &project, |store| {
        store.remove_library_dir(&name)?;
        store.remove_library_entry(&name).map_err(Into::into)
    })?;
    emit_mutation(
        &state,
        &project,
        topic::PROJECT,
        MutationDetail::LibraryRemoved { name },
    );
    Ok(Json(RunResponse { ok: true }))
}
