use std::fs;
use std::path::{Path, PathBuf};

use crate::errors::StoreError;
use crate::fsutil::write_atomic;
use crate::types::{
    Device, Edge, IoMap, ModbusConfig, PouFileSource, PouLanguage, PouType, ProgramInstance,
    ProjectManifest, ProjectTreeSkeleton, Protocol, ProtocolConfig, Task, Tasks,
};

const SAMPLE_MAIN_ST: &str = include_str!("../templates/main.st");

/// Cheap to clone (a path + the parsed manifest): the server clones one
/// out of its registry lock before compiling, so CPU-bound compiles never
/// run under the global projects mutex.
#[derive(Clone)]
pub struct ProjectStore {
    root: PathBuf,
    manifest: ProjectManifest,
}

impl ProjectStore {
    /// Open an existing project at `root`. Fails if `project.toml` is missing.
    ///
    /// Auto-migrates legacy projects: if `applications/` exists and `pous/`
    /// doesn't, the directory is renamed. The old name was a misnomer
    /// (Codesys uses "Application" for a deployment bundle of POUs+tasks+IO;
    /// here it just held POU source files) and the rename brings the
    /// on-disk layout into line with the rest of the model.
    pub fn open(root: PathBuf) -> Result<Self, StoreError> {
        let manifest_path = root.join("project.toml");
        if !manifest_path.exists() {
            return Err(StoreError::NotFound(root.display().to_string()));
        }
        let text = fs::read_to_string(&manifest_path)?;
        let manifest: ProjectManifest = toml::from_str(&text)?;
        // A semantically bad [governance] table (nan/inf bounds, min >
        // max, empty/duplicate variables) must be a loud load error —
        // never a policy that silently misbehaves on the scan thread.
        manifest.governance.validate().map_err(invalid_governance)?;
        let store = Self { root, manifest };
        store.migrate_applications_to_pous()?;
        Ok(store)
    }

    /// Rename `applications/` → `pous/` if needed. Idempotent.
    fn migrate_applications_to_pous(&self) -> Result<(), StoreError> {
        let old = self.root.join("applications");
        let new = self.root.join("pous");
        if old.exists() && !new.exists() {
            fs::rename(&old, &new)?;
            tracing::info!(
                project = %self.manifest.name,
                "migrated applications/ → pous/"
            );
        }
        Ok(())
    }

    /// Create a fresh project at `root`. Fails if `root` already contains a
    /// `project.toml`. Seeds the directory with a sample PROGRAM and an
    /// empty iomap.
    pub fn create(root: PathBuf, name: &str) -> Result<Self, StoreError> {
        validate_path(name)?;
        let manifest_path = root.join("project.toml");
        if manifest_path.exists() {
            return Err(StoreError::AlreadyExists(root.display().to_string()));
        }

        fs::create_dir_all(&root)?;
        fs::create_dir_all(root.join("pous"))?;
        fs::create_dir_all(root.join("devices"))?;
        fs::create_dir_all(root.join("edges"))?;

        let manifest = ProjectManifest {
            name: name.to_string(),
            version: "0.1".into(),
            governance: crate::types::WriteGovernance::default(),
        };
        write_atomic(&manifest_path, toml::to_string_pretty(&manifest)?)?;
        write_atomic(
            root.join("iomap.toml"),
            toml::to_string_pretty(&IoMap::default())?,
        )?;
        write_atomic(root.join("pous/main.st"), SAMPLE_MAIN_ST)?;
        // Seed tasks.toml with a single 100 ms task running `main`. Users
        // edit this via the Tasks pane.
        let seed_tasks = Tasks {
            tasks: vec![Task {
                name: "plc_task".into(),
                interval_ms: 100,
                priority: 1,
            }],
            programs: vec![ProgramInstance {
                instance: "main_inst".into(),
                program: "main".into(),
                task: "plc_task".into(),
            }],
        };
        write_atomic(
            root.join("tasks.toml"),
            toml::to_string_pretty(&seed_tasks)?,
        )?;

        Ok(Self { root, manifest })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The project's write-governance policy (`[governance]` in
    /// project.toml). Defaults to fully open when absent.
    ///
    /// This is the copy cached when the project was opened — callers
    /// that spawn a run should use [`read_governance`](Self::read_governance)
    /// so a file edit tightening the policy takes effect without
    /// re-opening the project.
    pub fn governance(&self) -> &crate::types::WriteGovernance {
        &self.manifest.governance
    }

    /// Re-read `[governance]` from project.toml on disk, validated the
    /// same way [`open`](Self::open) validates it — a bad table is a
    /// loud error, never a silently-open policy. Same per-call
    /// freshness contract as `read_tasks` / `read_alarms`.
    pub fn read_governance(&self) -> Result<crate::types::WriteGovernance, StoreError> {
        let text = fs::read_to_string(self.root.join("project.toml"))?;
        let manifest: ProjectManifest = toml::from_str(&text)?;
        manifest.governance.validate().map_err(invalid_governance)?;
        Ok(manifest.governance)
    }

    /// Project tree without parsed declarations — the server fills those
    /// in by calling `ironplc_bridge::extract_pou_declarations` on each
    /// source. Doing it this way keeps the project crate parser-free
    /// (no dependency on the bridge / ironplc).
    pub fn tree_skeleton(&self) -> Result<ProjectTreeSkeleton, StoreError> {
        let pou_paths = self.list_pou_paths()?;
        let mut pous = Vec::with_capacity(pou_paths.len());
        for path in pou_paths {
            let language = self.pou_file_language(&path)?;
            let source = self.read_pou_source(&path)?;
            pous.push(PouFileSource {
                path,
                source,
                language,
            });
        }
        let devices = self.list_devices()?;
        let edges = self.list_edges()?;
        let pou_folders = self.list_folders("pous")?;
        let device_folders = self.list_folders("devices")?;
        let edge_folders = self.list_folders("edges")?;
        let iomap = self.read_iomap()?;
        let tasks = self.read_tasks()?.unwrap_or_default();
        Ok(ProjectTreeSkeleton {
            name: self.manifest.name.clone(),
            path: self.root.display().to_string(),
            pous,
            pou_folders,
            devices,
            device_folders,
            edges,
            edge_folders,
            iomap,
            tasks,
        })
    }

    // ---------------- POU files ----------------
    //
    // The store deals in *files* — `pous/<path>.st`. POU *declarations*
    // (PROGRAM / FUNCTION_BLOCK / FUNCTION) inside each file are parser-
    // driven and exposed via the bridge in the server layer; the store
    // stays parser-free.

    // ============================================================
    //  HMI screens — hmi/<slug>.hmi.json (docs/hmi-design.md)
    // ============================================================

    /// Slugs of every screen under `hmi/`, sorted. Same identity rules as
    /// POU paths (slash-separated, no extension).
    pub fn list_hmis(&self) -> Result<Vec<String>, StoreError> {
        let root = self.root.join("hmi");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<(), StoreError> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    let next = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}/{name}")
                    };
                    walk(&path, &next, out)?;
                } else if let Some(slug) = name.strip_suffix(".hmi.json") {
                    out.push(if prefix.is_empty() {
                        slug.to_string()
                    } else {
                        format!("{prefix}/{slug}")
                    });
                }
            }
            Ok(())
        }
        walk(&root, "", &mut out)?;
        out.sort();
        Ok(out)
    }

    fn hmi_file(&self, path: &str) -> PathBuf {
        self.root.join("hmi").join(format!("{path}.hmi.json"))
    }

    /// Parse the screen at `path`. `HmiNotFound` when the file is absent;
    /// `HmiCorrupt` when it exists but fails JSON parse (the editor shows
    /// it — same philosophy as POU files that fail to parse).
    pub fn read_hmi(&self, path: &str) -> Result<crate::hmi::HmiDoc, StoreError> {
        validate_path(path)?;
        let file = self.hmi_file(path);
        if !file.exists() {
            return Err(StoreError::HmiNotFound(path.into()));
        }
        let raw = fs::read_to_string(&file)?;
        serde_json::from_str(&raw).map_err(|e| StoreError::HmiCorrupt(path.into(), e.to_string()))
    }

    /// Create a fresh empty screen. `AlreadyExists` if the slug is taken.
    pub fn create_hmi(&self, path: &str, title: &str) -> Result<crate::hmi::HmiDoc, StoreError> {
        validate_path(path)?;
        let file = self.hmi_file(path);
        if file.exists() {
            return Err(StoreError::AlreadyExists(path.into()));
        }
        let doc = crate::hmi::empty_hmi(title);
        self.write_hmi(path, &doc)?;
        Ok(doc)
    }

    /// Serialize + write the screen through [`write_atomic`], like every
    /// other project file — a crash mid-save must not corrupt a screen.
    pub fn write_hmi(&self, path: &str, doc: &crate::hmi::HmiDoc) -> Result<(), StoreError> {
        validate_path(path)?;
        let file = self.hmi_file(path);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(doc)
            .map_err(|e| StoreError::InvalidName(format!("serialize hmi: {e}")))?;
        write_atomic(&file, json)?;
        Ok(())
    }

    pub fn delete_hmi(&self, path: &str) -> Result<(), StoreError> {
        validate_path(path)?;
        let file = self.hmi_file(path);
        if !file.exists() {
            return Err(StoreError::HmiNotFound(path.into()));
        }
        fs::remove_file(file)?;
        Ok(())
    }

    pub fn list_pou_paths(&self) -> Result<Vec<String>, StoreError> {
        let root = self.root.join("pous");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        walk_pou_files(&root, "", &mut |slug, _lang, _src| {
            out.push(slug.to_string());
            Ok(())
        })?;
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Locate the on-disk file for a POU slug, returning both the path
    /// and the language detected from its extension. We probe `.st`
    /// first (the common case) then `.ld.json`. When graphical languages
    /// expand we add more probes here — one source of truth for the
    /// "slug → file" mapping.
    pub fn pou_file_path(&self, slug: &str) -> Option<(PathBuf, PouLanguage)> {
        let dir = self.root.join("pous");
        let st = dir.join(format!("{slug}.st"));
        if st.exists() {
            return Some((st, PouLanguage::St));
        }
        let ld = dir.join(format!("{slug}.ld.json"));
        if ld.exists() {
            return Some((ld, PouLanguage::Ld));
        }
        let fbd = dir.join(format!("{slug}.fbd.json"));
        if fbd.exists() {
            return Some((fbd, PouLanguage::Fbd));
        }
        let sfc = dir.join(format!("{slug}.sfc.json"));
        if sfc.exists() {
            return Some((sfc, PouLanguage::Sfc));
        }
        None
    }

    /// Read the raw on-disk contents of a POU file, regardless of
    /// language. For `.st` this is the IEC source; for `.ld.json` it's
    /// the JSON literal — the caller (typically the bridge) decides
    /// what to do with it based on the language hint from
    /// [`pou_file_language`](Self::pou_file_language).
    pub fn read_pou_source(&self, path: &str) -> Result<String, StoreError> {
        validate_path(path)?;
        let (file, _lang) = self
            .pou_file_path(path)
            .ok_or_else(|| StoreError::PouNotFound(path.into()))?;
        Ok(fs::read_to_string(&file)?)
    }

    /// Read the language of an existing POU. Returns `PouNotFound` if
    /// the slug doesn't resolve to a file on disk.
    pub fn pou_file_language(&self, path: &str) -> Result<PouLanguage, StoreError> {
        validate_path(path)?;
        let (_, lang) = self
            .pou_file_path(path)
            .ok_or_else(|| StoreError::PouNotFound(path.into()))?;
        Ok(lang)
    }

    /// Overwrite the POU file at `path`. The on-disk language is
    /// preserved — we don't allow a save to change `.st` into `.ld.json`
    /// or vice versa, because that would silently invalidate any
    /// in-flight UI state. Creating a new POU of a different language
    /// is `create_pou_file`.
    pub fn write_pou_source(&self, path: &str, source: &str) -> Result<(), StoreError> {
        validate_path(path)?;
        let (file, _lang) = match self.pou_file_path(path) {
            Some(pair) => pair,
            None => {
                // First write of a freshly-named POU — fall back to
                // `.st`. New non-ST POUs come through `create_pou_file`
                // which sets the extension explicitly.
                let p = self.root.join("pous").join(format!("{path}.st"));
                (p, PouLanguage::St)
            }
        };
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        write_atomic(file, source)?;
        Ok(())
    }

    /// Create a new POU file with a single declaration of `type_` in
    /// `language`. Returns the seeded source.
    ///
    /// Supported languages: ST and LD. Others (FBD / SFC / IL / CFC)
    /// still error out cleanly so the UI can surface a "coming soon"
    /// message rather than silently writing a file the bridge can't
    /// compile.
    pub fn create_pou_file(
        &self,
        path: &str,
        type_: PouType,
        language: PouLanguage,
    ) -> Result<String, StoreError> {
        validate_path(path)?;
        if self.pou_file_path(path).is_some() {
            return Err(StoreError::AlreadyExists(path.into()));
        }
        let leaf = leaf_segment(path);
        let (file, source) = match language {
            PouLanguage::St => (
                self.root.join("pous").join(format!("{path}.st")),
                template_for(leaf, type_),
            ),
            PouLanguage::Ld => (
                self.root.join("pous").join(format!("{path}.ld.json")),
                template_for_ld(leaf, type_),
            ),
            PouLanguage::Fbd => (
                self.root.join("pous").join(format!("{path}.fbd.json")),
                template_for_fbd(leaf, type_),
            ),
            PouLanguage::Sfc => (
                self.root.join("pous").join(format!("{path}.sfc.json")),
                template_for_sfc(leaf, type_),
            ),
        };
        fs::create_dir_all(file.parent().unwrap())?;
        write_atomic(&file, &source)?;
        Ok(source)
    }

    pub fn delete_pou_file(&self, path: &str) -> Result<(), StoreError> {
        validate_path(path)?;
        let (file, _lang) = self
            .pou_file_path(path)
            .ok_or_else(|| StoreError::PouNotFound(path.into()))?;
        fs::remove_file(file)?;
        Ok(())
    }

    pub fn create_pou_folder(&self, path: &str) -> Result<(), StoreError> {
        self.create_folder("pous", path)
    }

    pub fn delete_pou_folder(&self, path: &str) -> Result<(), StoreError> {
        self.delete_folder("pous", path)
    }

    // ---------------- Libraries ----------------
    //
    // Imported library blocks are vendored under `pous/lib/<library>/`
    // — still ordinary POU files (the walk, compile and deploy paths
    // need no special casing; an edge deploy stays a self-contained
    // project) but in a reserved subtree the API write-protects and
    // the IDE renders as a separate read-only "Libraries" section.
    // Which libraries (and versions) were imported is recorded in
    // project.toml's `[libraries]` table.

    /// `[libraries]` table from project.toml: library name → version.
    /// Read fresh from disk each call — the file is tiny and this keeps
    /// the in-memory manifest immutable.
    pub fn read_libraries(&self) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        let table = self.read_manifest_table()?;
        let mut out = std::collections::BTreeMap::new();
        if let Some(toml::Value::Table(libs)) = table.get("libraries") {
            for (k, v) in libs {
                if let toml::Value::String(ver) = v {
                    out.insert(k.clone(), ver.clone());
                }
            }
        }
        Ok(out)
    }

    /// Record (or version-bump) a library in project.toml. Preserves
    /// every other key in the manifest verbatim.
    pub fn set_library_entry(&self, library: &str, version: &str) -> Result<(), StoreError> {
        validate_single_segment(library)?;
        let mut table = self.read_manifest_table()?;
        let libs = table
            .entry("libraries".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(libs) = libs {
            libs.insert(
                library.to_string(),
                toml::Value::String(version.to_string()),
            );
        }
        self.write_manifest_table(&table)
    }

    /// Drop a library's entry from project.toml. Removing the last one
    /// removes the `[libraries]` table entirely.
    pub fn remove_library_entry(&self, library: &str) -> Result<(), StoreError> {
        let mut table = self.read_manifest_table()?;
        if let Some(toml::Value::Table(libs)) = table.get_mut("libraries") {
            libs.remove(library);
            if libs.is_empty() {
                table.remove("libraries");
            }
        }
        self.write_manifest_table(&table)
    }

    /// Write one imported block file under `pous/lib/<library>/`.
    /// `file_name` is the bare on-disk name (e.g. `fb_pid.st`).
    pub fn write_library_file(
        &self,
        library: &str,
        file_name: &str,
        content: &str,
    ) -> Result<(), StoreError> {
        validate_single_segment(library)?;
        validate_single_segment(file_name)?;
        let dir = self
            .root
            .join("pous")
            .join(LIBRARY_SLUG_PREFIX)
            .join(library);
        fs::create_dir_all(&dir)?;
        write_atomic(dir.join(file_name), content)?;
        Ok(())
    }

    /// Remove `pous/lib/<library>/` and everything in it; prunes the
    /// now-empty `pous/lib/` too so an empty shell doesn't linger in
    /// the tree. Missing dir is fine — removal is idempotent.
    pub fn remove_library_dir(&self, library: &str) -> Result<(), StoreError> {
        validate_single_segment(library)?;
        let lib_root = self.root.join("pous").join(LIBRARY_SLUG_PREFIX);
        let dir = lib_root.join(library);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        if lib_root.exists() && fs::read_dir(&lib_root)?.next().is_none() {
            fs::remove_dir(&lib_root)?;
        }
        Ok(())
    }

    fn read_manifest_table(&self) -> Result<toml::Table, StoreError> {
        let text = fs::read_to_string(self.root.join("project.toml"))?;
        Ok(toml::from_str(&text)?)
    }

    fn write_manifest_table(&self, table: &toml::Table) -> Result<(), StoreError> {
        write_atomic(
            self.root.join("project.toml"),
            toml::to_string_pretty(table)?,
        )?;
        Ok(())
    }

    // ---------------- Devices ----------------

    pub fn list_devices(&self) -> Result<Vec<Device>, StoreError> {
        let root = self.root.join("devices");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        walk_files(&root, "", "toml", &mut |rel, text| {
            let mut device: Device = toml::from_str(&text)?;
            // Honour the on-disk location as the canonical name. Old single-
            // segment files keep their existing name (rel == device.name);
            // nested files get the folder-qualified path so iomap mapping
            // can target the right device unambiguously.
            device.name = rel.to_string();
            out.push(device);
            Ok(())
        })?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn read_device(&self, name: &str) -> Result<Device, StoreError> {
        validate_path(name)?;
        let path = self.root.join("devices").join(format!("{name}.toml"));
        if !path.exists() {
            return Err(StoreError::DeviceNotFound(name.into()));
        }
        let text = fs::read_to_string(&path)?;
        let mut device: Device = toml::from_str(&text)?;
        device.name = name.into();
        Ok(device)
    }

    pub fn write_device(&self, device: &Device) -> Result<(), StoreError> {
        validate_path(&device.name)?;
        let leaf_name = leaf_segment(&device.name).to_string();
        let path = self
            .root
            .join("devices")
            .join(format!("{}.toml", device.name));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Serialised form stores just the leaf — the folder path is implicit
        // from the file location, and double-encoding it would confuse old
        // top-level loaders. The list/read paths overwrite `name` with the
        // full path again.
        let on_disk = Device {
            name: leaf_name,
            config: device.config.clone(),
        };
        write_atomic(path, toml::to_string_pretty(&on_disk)?)?;
        Ok(())
    }

    pub fn create_device(&self, name: &str, protocol: Protocol) -> Result<Device, StoreError> {
        validate_path(name)?;
        let path = self.root.join("devices").join(format!("{name}.toml"));
        if path.exists() {
            return Err(StoreError::AlreadyExists(name.into()));
        }
        let config = default_config_for(protocol);
        let device = Device {
            name: name.into(),
            config,
        };
        self.write_device(&device)?;
        Ok(device)
    }

    pub fn delete_device(&self, name: &str) -> Result<(), StoreError> {
        validate_path(name)?;
        let path = self.root.join("devices").join(format!("{name}.toml"));
        if !path.exists() {
            return Err(StoreError::DeviceNotFound(name.into()));
        }
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn create_device_folder(&self, path: &str) -> Result<(), StoreError> {
        self.create_folder("devices", path)
    }

    // ---------------- Edges (deploy targets) ----------------

    pub fn list_edges(&self) -> Result<Vec<Edge>, StoreError> {
        let root = self.root.join("edges");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        walk_files(&root, "", "toml", &mut |rel, text| {
            let mut edge: Edge = toml::from_str(&text)?;
            edge.name = rel.to_string();
            out.push(edge);
            Ok(())
        })?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn read_edge(&self, name: &str) -> Result<Edge, StoreError> {
        validate_path(name)?;
        let path = self.root.join("edges").join(format!("{name}.toml"));
        if !path.exists() {
            return Err(StoreError::EdgeNotFound(name.into()));
        }
        let text = fs::read_to_string(&path)?;
        let mut edge: Edge = toml::from_str(&text)?;
        edge.name = name.into();
        validate_edge_target(&edge)?;
        Ok(edge)
    }

    pub fn write_edge(&self, edge: &Edge) -> Result<(), StoreError> {
        validate_path(&edge.name)?;
        validate_edge_target(edge)?;
        let leaf_name = leaf_segment(&edge.name).to_string();
        let path = self.root.join("edges").join(format!("{}.toml", edge.name));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let on_disk = Edge {
            name: leaf_name,
            ..edge.clone()
        };
        write_atomic(path, toml::to_string_pretty(&on_disk)?)?;
        Ok(())
    }

    pub fn create_edge(&self, name: &str, host: &str) -> Result<Edge, StoreError> {
        validate_path(name)?;
        validate_ssh_target("host", host, false)?;
        let path = self.root.join("edges").join(format!("{name}.toml"));
        if path.exists() {
            return Err(StoreError::AlreadyExists(name.into()));
        }
        let edge = Edge {
            name: name.into(),
            host: host.into(),
            ssh_port: 22,
            ssh_user: String::new(),
            install_dir: "/opt/ia2".into(),
            runtime_port: 13001,
            notes: String::new(),
        };
        self.write_edge(&edge)?;
        Ok(edge)
    }

    pub fn delete_edge(&self, name: &str) -> Result<(), StoreError> {
        validate_path(name)?;
        let path = self.root.join("edges").join(format!("{name}.toml"));
        if !path.exists() {
            return Err(StoreError::EdgeNotFound(name.into()));
        }
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn create_edge_folder(&self, path: &str) -> Result<(), StoreError> {
        self.create_folder("edges", path)
    }

    // ---------------- Folder helpers ----------------

    /// All subdirectories under `subdir` (e.g. "applications" / "devices"),
    /// returned as forward-slash separated relative paths. Includes empty
    /// folders — the UI needs them so it can render a folder before any
    /// items live inside.
    pub fn list_folders(&self, subdir: &str) -> Result<Vec<String>, StoreError> {
        let root = self.root.join(subdir);
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        walk_dirs(&root, "", &mut |rel| {
            out.push(rel.to_string());
            Ok(())
        })?;
        out.sort();
        Ok(out)
    }

    fn create_folder(&self, subdir: &str, path: &str) -> Result<(), StoreError> {
        validate_path(path)?;
        let dir = self.root.join(subdir).join(path);
        if dir.exists() {
            return Err(StoreError::FolderExists(path.into()));
        }
        fs::create_dir_all(&dir)?;
        Ok(())
    }

    pub fn delete_device_folder(&self, path: &str) -> Result<(), StoreError> {
        self.delete_folder("devices", path)
    }

    pub fn delete_edge_folder(&self, path: &str) -> Result<(), StoreError> {
        self.delete_folder("edges", path)
    }

    /// Remove a folder under `subdir`. Requires the folder to be empty
    /// (no .st / .toml children, no sub-folders) — agents and humans
    /// alike should delete contents explicitly first so an accidental
    /// recursive wipe isn't possible via a single API call.
    fn delete_folder(&self, subdir: &str, path: &str) -> Result<(), StoreError> {
        validate_path(path)?;
        let dir = self.root.join(subdir).join(path);
        if !dir.exists() {
            return Err(StoreError::FolderNotFound(path.into()));
        }
        let mut entries = fs::read_dir(&dir)?;
        if entries.next().is_some() {
            return Err(StoreError::FolderNotEmpty(path.into()));
        }
        fs::remove_dir(&dir)?;
        Ok(())
    }

    // ---------------- IO Mapping ----------------

    pub fn read_iomap(&self) -> Result<IoMap, StoreError> {
        let path = self.root.join("iomap.toml");
        if !path.exists() {
            return Ok(IoMap::default());
        }
        let text = fs::read_to_string(&path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn write_iomap(&self, iomap: &IoMap) -> Result<(), StoreError> {
        let path = self.root.join("iomap.toml");
        write_atomic(path, toml::to_string_pretty(iomap)?)?;
        Ok(())
    }

    // ---------------- Tasks (project-level scheduling) ----------------

    /// Read tasks.toml. Returns `None` when the file doesn't exist — that
    /// distinction matters: a fresh open of an old project needs to know
    /// the file is missing so it can offer migration.
    pub fn read_tasks(&self) -> Result<Option<Tasks>, StoreError> {
        let path = self.root.join("tasks.toml");
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&path)?;
        Ok(Some(toml::from_str(&text)?))
    }

    pub fn write_tasks(&self, tasks: &Tasks) -> Result<(), StoreError> {
        let path = self.root.join("tasks.toml");
        write_atomic(path, toml::to_string_pretty(tasks)?)?;
        Ok(())
    }

    // ---------------- Alarms (alarms.toml) ----------------

    /// Read alarms.toml. Missing file = no alarms configured (the
    /// default for projects that haven't authored any yet).
    pub fn read_alarms(&self) -> Result<crate::types::AlarmConfig, StoreError> {
        let path = self.root.join("alarms.toml");
        if !path.exists() {
            return Ok(crate::types::AlarmConfig::default());
        }
        let text = fs::read_to_string(&path)?;
        let config: crate::types::AlarmConfig = toml::from_str(&text)?;
        validate_alarm_ids(&config)?;
        Ok(config)
    }

    pub fn write_alarms(&self, alarms: &crate::types::AlarmConfig) -> Result<(), StoreError> {
        validate_alarm_ids(alarms)?;
        let path = self.root.join("alarms.toml");
        write_atomic(path, toml::to_string_pretty(alarms)?)?;
        Ok(())
    }

    // ---------------- Northbound (northbound.toml) ----------------

    /// Read northbound.toml. Missing file = northbound disabled (the
    /// common case for projects that don't publish to a platform).
    pub fn read_northbound(&self) -> Result<crate::types::NorthboundConfig, StoreError> {
        let path = self.root.join("northbound.toml");
        if !path.exists() {
            return Ok(crate::types::NorthboundConfig::default());
        }
        let text = fs::read_to_string(&path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn write_northbound(
        &self,
        config: &crate::types::NorthboundConfig,
    ) -> Result<(), StoreError> {
        let path = self.root.join("northbound.toml");
        write_atomic(path, toml::to_string_pretty(config)?)?;
        Ok(())
    }

    /// Auto-migrate an old project from per-POU inline CONFIGURATION to a
    /// project-level `tasks.toml`. Idempotent — if `tasks.toml` already
    /// exists this returns `Ok(MigrationReport::Skipped)`.
    ///
    /// Strategy: scan every POU file for `CONFIGURATION ... END_CONFIGURATION`
    /// blocks; extract the TASK definitions and `PROGRAM <inst> WITH <task>`
    /// bindings; merge into a single `Tasks` record; write `tasks.toml`;
    /// strip the CONFIGURATION blocks out of the POU files in place (git
    /// is the backup).
    pub fn migrate_tasks(&self) -> Result<MigrationReport, StoreError> {
        if self.read_tasks()?.is_some() {
            return Ok(MigrationReport::Skipped);
        }
        let pou_paths = self.list_pou_paths()?;
        let mut tasks: Vec<Task> = Vec::new();
        let mut programs: Vec<ProgramInstance> = Vec::new();
        let mut stripped: Vec<String> = Vec::new();
        let mut seen_task_names = std::collections::HashSet::<String>::new();
        let mut seen_instance_names = std::collections::HashSet::<String>::new();

        for path in &pou_paths {
            let source = self.read_pou_source(path)?;
            let extracted = extract_inline_configuration(&source);
            if extracted.is_empty() {
                continue;
            }
            // Strip every CONFIGURATION block from this POU.
            let stripped_source = strip_inline_configuration(&source);
            self.write_pou_source(path, &stripped_source)?;
            stripped.push(path.clone());

            for inline in extracted {
                for t in inline.tasks {
                    // Dedup by task name: first wins. Subsequent POUs that
                    // declared the same task name keep their PROGRAM
                    // instances mapped to it.
                    if seen_task_names.insert(t.name.clone()) {
                        tasks.push(t);
                    }
                }
                for p in inline.programs {
                    if seen_instance_names.insert(p.instance.clone()) {
                        programs.push(p);
                    }
                }
            }
        }

        // Seed defaults when migration found nothing to lift (e.g. project
        // with FB-only POUs that never had CONFIGURATION blocks). The user
        // can edit the Tasks pane to actually bind a program.
        if tasks.is_empty() {
            tasks.push(Task {
                name: "plc_task".into(),
                interval_ms: 100,
                priority: 1,
            });
        }
        let result = Tasks { tasks, programs };
        self.write_tasks(&result)?;
        Ok(MigrationReport::Migrated {
            tasks_count: result.tasks.len(),
            programs_count: result.programs.len(),
            pous_modified: stripped,
        })
    }
}

/// Pick a reasonable NIC default for EtherCAT. The user almost always
/// overrides this in the editor; we just want to avoid an empty box.
fn default_ethercat_nic() -> &'static str {
    if cfg!(target_os = "macos") {
        "en0"
    } else if cfg!(target_os = "windows") {
        "Ethernet"
    } else {
        "eth0"
    }
}

/// Outcome of `ProjectStore::migrate_tasks`.
#[derive(Debug, Clone)]
pub enum MigrationReport {
    /// `tasks.toml` already exists; no work done.
    Skipped,
    /// Inline CONFIGURATION blocks were extracted into `tasks.toml` and
    /// stripped from the POUs listed in `pous_modified`.
    Migrated {
        tasks_count: usize,
        programs_count: usize,
        pous_modified: Vec<String>,
    },
}

/// Tasks + program bindings extracted from one POU's inline CONFIGURATION
/// block. Internal-only intermediate type for migration.
struct InlineConfig {
    tasks: Vec<Task>,
    programs: Vec<ProgramInstance>,
}

/// Find every `CONFIGURATION … END_CONFIGURATION` block in a source file and
/// extract the TASK declarations + PROGRAM instance bindings. Best-effort
/// regex-free parsing — exact enough for legacy projects written by our
/// own template, tolerant of whitespace and case.
fn extract_inline_configuration(source: &str) -> Vec<InlineConfig> {
    let mut out = Vec::new();
    let lower = source.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(start) = lower[cursor..].find("configuration ") {
        let abs = cursor + start;
        let Some(end) = lower[abs..].find("end_configuration") else {
            break;
        };
        let abs_end = abs + end + "end_configuration".len();
        let block = &source[abs..abs_end];
        out.push(parse_configuration_block(block));
        cursor = abs_end;
    }
    out
}

/// Replace every `CONFIGURATION … END_CONFIGURATION` block in `source`
/// with an empty line so the rest of the file stays intact.
fn strip_inline_configuration(source: &str) -> String {
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    while let Some(start_rel) = lower[cursor..].find("configuration ") {
        let abs = cursor + start_rel;
        out.push_str(&source[cursor..abs]);
        let Some(end_rel) = lower[abs..].find("end_configuration") else {
            // No matching close — fall back to copying the rest verbatim.
            out.push_str(&source[abs..]);
            cursor = source.len();
            break;
        };
        let abs_end = abs + end_rel + "end_configuration".len();
        cursor = abs_end;
        // Skip a trailing newline so we don't leave a blank line behind.
        if source.as_bytes().get(cursor).copied() == Some(b'\n') {
            cursor += 1;
        }
    }
    out.push_str(&source[cursor..]);
    out
}

/// Pull TASK and PROGRAM-instance declarations out of a single
/// CONFIGURATION block. Tolerates the formatting our own template
/// emits; not a full IEC parser — anything fancy lands in `tasks.toml`
/// as a no-op that the user can fix by hand.
fn parse_configuration_block(block: &str) -> InlineConfig {
    let mut tasks = Vec::new();
    let mut programs = Vec::new();
    for raw_line in block.lines() {
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("task ") {
            // TASK <name>(INTERVAL := T#<spec>, PRIORITY := <num>);
            if let Some((name, args)) = split_paren(rest) {
                let interval_ms = parse_interval_ms(&args).unwrap_or(100);
                let priority = parse_priority(&args).unwrap_or(1);
                tasks.push(Task {
                    name: name.trim().to_string(),
                    interval_ms,
                    priority,
                });
            }
        } else if let Some(rest) = lower.strip_prefix("program ") {
            // PROGRAM <instance> WITH <task> : <program_type>;
            // Split by " with " then by " : ", case-insensitive.
            if let Some(with_idx) = rest.find(" with ") {
                let instance = rest[..with_idx].trim().to_string();
                let after = &rest[with_idx + 6..];
                if let Some(colon_idx) = after.find(':') {
                    let task = after[..colon_idx].trim().to_string();
                    let prog = after[colon_idx + 1..]
                        .trim()
                        .trim_end_matches(';')
                        .trim()
                        .to_string();
                    programs.push(ProgramInstance {
                        instance,
                        program: prog,
                        task,
                    });
                }
            }
        }
    }
    InlineConfig { tasks, programs }
}

fn split_paren(s: &str) -> Option<(String, String)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    if close < open {
        return None;
    }
    Some((s[..open].to_string(), s[open + 1..close].to_string()))
}

fn parse_interval_ms(args: &str) -> Option<u32> {
    // INTERVAL := T#<spec>
    let lower = args.to_ascii_lowercase();
    let idx = lower.find("interval")?;
    let after = &args[idx..];
    let assign = after.find(":=")?;
    let value = after[assign + 2..]
        .trim_start()
        .split([',', ')'])
        .next()?
        .trim();
    // Accept T#100ms, T#1s, T#1m, LTIME#…, etc.
    let stripped = value
        .trim_start_matches('T')
        .trim_start_matches('t')
        .trim_start_matches("LTIME")
        .trim_start_matches("ltime")
        .trim_start_matches('#');
    parse_time_to_ms(stripped)
}

fn parse_time_to_ms(spec: &str) -> Option<u32> {
    // Very tolerant: pull <num><unit> tokens and sum them up. Units we
    // recognise: ms, s, m, h. Anything unknown is ignored (0).
    let mut total: u32 = 0;
    let mut num = String::new();
    let mut unit = String::new();
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '_' {
            if c.is_ascii_digit() {
                num.push(c);
            }
        } else if c.is_alphabetic() {
            unit.push(c.to_ascii_lowercase());
            while let Some(&p) = chars.peek() {
                if p.is_alphabetic() {
                    unit.push(p.to_ascii_lowercase());
                    chars.next();
                } else {
                    break;
                }
            }
            let n: u32 = num.parse().ok()?;
            let factor: u32 = match unit.as_str() {
                "ms" => 1,
                "s" => 1_000,
                "m" => 60_000,
                "h" => 3_600_000,
                _ => 0,
            };
            total = total.saturating_add(n.saturating_mul(factor));
            num.clear();
            unit.clear();
        }
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

fn parse_priority(args: &str) -> Option<i32> {
    let lower = args.to_ascii_lowercase();
    let idx = lower.find("priority")?;
    let after = &args[idx..];
    let assign = after.find(":=")?;
    let value = after[assign + 2..]
        .trim_start()
        .split([',', ')'])
        .next()?
        .trim();
    value.parse().ok()
}

/// Reserved first segment of the POU slug space holding imported
/// library blocks (`pous/lib/<library>/...`). The generic POU CRUD
/// routes refuse to write here; the library import/remove API is the
/// only mutator.
pub const LIBRARY_SLUG_PREFIX: &str = "lib";

/// True when a POU slug (or folder path) lives in the reserved library
/// subtree.
pub fn is_library_slug(slug: &str) -> bool {
    slug == LIBRARY_SLUG_PREFIX || slug.starts_with("lib/")
}

/// Wrap a `WriteGovernance::validate` failure as a load error. The
/// manifest PARSED — the table is semantically invalid — but load time
/// is the contract boundary, and `TomlDe` is the established
/// "project.toml is bad" error, so we mint one via serde's custom
/// constructor rather than widening the error enum.
fn invalid_governance(msg: String) -> StoreError {
    use serde::de::Error as _;
    StoreError::TomlDe(toml::de::Error::custom(format!(
        "[governance] is invalid: {msg}"
    )))
}

/// A single path segment: no separators at all, plus the usual
/// `validate_path` per-segment rules.
/// Reject `host` / `ssh_user` values that `ssh` would not treat as a plain
/// destination.
///
/// These are passed to `ssh` as the destination argument (`host` or
/// `user@host`), never through a shell — so shell metacharacters are inert,
/// but **argument** injection is not. `ssh` parses any argument starting with
/// `-` as an option, so a host of `-oProxyCommand=<cmd>` makes the dev
/// machine run `<cmd>` on the next probe / deploy / attach. Verified against
/// OpenSSH 10.3: the ProxyCommand ran.
///
/// The reachable path is not theoretical: `POST /api/edges` takes `host`
/// straight from the request body, `PUT` writes a whole `Edge`, and both land
/// in `edges/<name>.toml` — a file that also travels with a shared or
/// agent-authored project (ADR-0002's threat model).
///
/// Deliberately permissive otherwise: `~/.ssh/config` aliases, bare IPv6
/// literals and ordinary hostnames all pass. Only a leading `-`, whitespace,
/// and control characters are refused.
fn validate_ssh_target(
    field: &'static str,
    value: &str,
    allow_empty: bool,
) -> Result<(), StoreError> {
    let bad = |why: &'static str| {
        Err(StoreError::InvalidEdgeTarget {
            field,
            value: value.into(),
            why,
        })
    };
    if value.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            bad("it is empty")
        };
    }
    if value.starts_with('-') {
        return bad("it starts with '-'");
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("it contains whitespace or control characters");
    }
    Ok(())
}

/// Both halves of an edge's ssh destination. Applied on read as well as
/// write: a project whose `edges/*.toml` was authored outside the API must
/// not load an unusable destination and hand it to `ssh`.
fn validate_edge_target(edge: &Edge) -> Result<(), StoreError> {
    validate_ssh_target("host", &edge.host, false)?;
    validate_ssh_target("ssh_user", &edge.ssh_user, true)
}

fn validate_alarm_ids(config: &crate::types::AlarmConfig) -> Result<(), StoreError> {
    if let Some(def) = config
        .alarms
        .iter()
        .find(|d| d.id.starts_with(crate::DEVICE_ALARM_PREFIX))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "alarm id '{}' uses the reserved device-health prefix '{}'",
                def.id,
                crate::DEVICE_ALARM_PREFIX
            ),
        )
        .into());
    }
    Ok(())
}

fn validate_single_segment(name: &str) -> Result<(), StoreError> {
    if name.contains('/') {
        return Err(StoreError::InvalidName(name.into()));
    }
    validate_path(name)
}

/// Validate a project-relative path. Accepts forward-slash separated
/// segments (e.g. `pid_loops/temperature_pid`). Rejects empty segments,
/// dot-prefixed segments (no hidden files, no `..` traversal), and
/// platform-special characters that confuse cross-platform path joining.
fn validate_path(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::InvalidName(name.into()));
    }
    for segment in name.split('/') {
        if segment.is_empty()
            || segment.starts_with('.')
            || segment.contains('\\')
            || segment.contains(':')
        {
            return Err(StoreError::InvalidName(name.into()));
        }
    }
    Ok(())
}

/// Last segment of a slash-separated path; the IEC POU identifier name.
fn leaf_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Recursively walk pou files under `root`, recognising both `.st`
/// and `.ld.json` extensions. The slug returned strips the extension(s)
/// so callers can pass it back through `pou_file_path` / `read_pou_*`.
fn walk_pou_files(
    root: &Path,
    prefix: &str,
    cb: &mut dyn FnMut(&str, PouLanguage, String) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        if ftype.is_dir() {
            let next_prefix = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            walk_pou_files(&path, &next_prefix, cb)?;
            continue;
        }
        if !ftype.is_file() {
            continue;
        }
        // Match the longest known suffix first so `.ld.json` doesn't
        // get classified as a plain `.json`.
        let (slug_stem, lang) = if let Some(s) = name.strip_suffix(".ld.json") {
            (s, PouLanguage::Ld)
        } else if let Some(s) = name.strip_suffix(".fbd.json") {
            (s, PouLanguage::Fbd)
        } else if let Some(s) = name.strip_suffix(".sfc.json") {
            (s, PouLanguage::Sfc)
        } else if let Some(s) = name.strip_suffix(".st") {
            (s, PouLanguage::St)
        } else {
            continue;
        };
        let slug = if prefix.is_empty() {
            slug_stem.to_string()
        } else {
            format!("{prefix}/{slug_stem}")
        };
        let contents = fs::read_to_string(&path)?;
        cb(&slug, lang, contents)?;
    }
    Ok(())
}

/// Recursively walk every file under `root` whose extension matches `ext`,
/// invoking `cb(rel_path_without_ext, contents)` for each.
fn walk_files(
    root: &Path,
    prefix: &str,
    ext: &str,
    cb: &mut dyn FnMut(&str, String) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        if ftype.is_dir() {
            let next_prefix = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            walk_files(&path, &next_prefix, ext, cb)?;
        } else if ftype.is_file() && path.extension().and_then(|s| s.to_str()) == Some(ext) {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            let rel = if prefix.is_empty() {
                stem.to_string()
            } else {
                format!("{prefix}/{stem}")
            };
            let contents = fs::read_to_string(&path)?;
            cb(&rel, contents)?;
        }
    }
    Ok(())
}

/// Recursively walk every directory under `root`, emitting their forward-
/// slash relative paths via `cb`.
fn walk_dirs(
    root: &Path,
    prefix: &str,
    cb: &mut dyn FnMut(&str) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(s) => s,
            None => continue,
        };
        if name.starts_with('.') || !ftype.is_dir() {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        cb(&rel)?;
        walk_dirs(&path, &rel, cb)?;
    }
    Ok(())
}

/// Source template for a new single-POU file. We bake the IEC POU
/// header + an empty VAR block; the runtime's compile path enables
/// `allow_empty_var_blocks` so this is valid input.
fn template_for(name: &str, type_: PouType) -> String {
    match type_ {
        PouType::Program => format!(
            "PROGRAM {name}\n    VAR\n    END_VAR\n\n    \
             (* Add your program logic here. Bind this PROGRAM to a task\n       \
             in the Tasks pane to actually run it. *)\n\nEND_PROGRAM\n"
        ),
        PouType::FunctionBlock => format!(
            "FUNCTION_BLOCK {name}\n    VAR_INPUT\n    END_VAR\n    \
             VAR_OUTPUT\n    END_VAR\n    VAR\n    END_VAR\n\n    \
             (* Add your FB logic here *)\n\nEND_FUNCTION_BLOCK\n"
        ),
        PouType::Function => format!(
            "FUNCTION {name} : INT\n    VAR_INPUT\n    END_VAR\n    \
             VAR\n    END_VAR\n\n    \
             (* Return a value via the function name: {name} := ... *)\n\
             \nEND_FUNCTION\n"
        ),
    }
}

/// Source template for a new LD POU. Returns a JSON literal that
/// matches the [`crate::ld::LdProgram`] schema — one always-passing
/// rung that asserts the coil, so the new file compiles end-to-end
/// without further authoring. The user replaces the rung with their
/// actual logic.
fn template_for_ld(name: &str, type_: PouType) -> String {
    use crate::ld::{
        LdCoil, LdCoilKind, LdNode, LdPouType, LdProgram, LdRung, LdVarSection, LdVariable,
    };
    let pou_type = match type_ {
        PouType::Program => LdPouType::Program,
        PouType::FunctionBlock => LdPouType::FunctionBlock,
        // Functions in LD are uncommon and not exposed in the UI yet
        // (see `LdPouType` docs). Fall back to Program so the seeded
        // file is at least valid; the caller is responsible for
        // gating Function creation in LD.
        PouType::Function => LdPouType::Program,
    };
    let prog = LdProgram {
        name: name.into(),
        pou_type,
        variables: vec![LdVariable {
            name: "out".into(),
            type_name: "BOOL".into(),
            section: LdVarSection::Internal,
            init: None,
        }],
        rungs: vec![LdRung {
            id: "r0".into(),
            label: Some("Replace this rung with your logic".into()),
            logic: LdNode::Const { value: true },
            coils: vec![LdCoil {
                var: "out".into(),
                kind: LdCoilKind::Standard,
            }],
        }],
    };
    serde_json::to_string_pretty(&prog).expect("LD template serialises")
}

fn template_for_fbd(name: &str, type_: PouType) -> String {
    use crate::fbd::{FbdBlock, FbdInputBinding, FbdInputSource, FbdProgram};
    use crate::ld::{LdPouType, LdVarSection, LdVariable};
    let pou_type = match type_ {
        PouType::Program => LdPouType::Program,
        PouType::FunctionBlock => LdPouType::FunctionBlock,
        // FBD functions are also uncommon — same fallback as LD.
        PouType::Function => LdPouType::Program,
    };
    // Seed with a single TON block driven by a placeholder input so
    // the file is a valid, compileable starting point — the user sees
    // the canonical shape immediately and edits one block / one wire
    // at a time. We don't bind an output (no VAR_OUTPUT in this
    // skeleton), so the block is a no-op at runtime but renders + lints
    // clean.
    let prog = FbdProgram {
        name: name.into(),
        pou_type,
        variables: vec![LdVariable {
            name: "trigger".into(),
            type_name: "BOOL".into(),
            section: LdVarSection::Internal,
            init: None,
        }],
        blocks: vec![FbdBlock {
            id: "b0".into(),
            fb_type: "TON".into(),
            instance: "myT".into(),
            inputs: vec![
                FbdInputBinding {
                    pin: "IN".into(),
                    value: FbdInputSource::Var {
                        name: "trigger".into(),
                    },
                },
                FbdInputBinding {
                    pin: "PT".into(),
                    value: FbdInputSource::Literal {
                        value: "T#1s".into(),
                    },
                },
            ],
            position: None,
        }],
        outputs: vec![],
    };
    serde_json::to_string_pretty(&prog).expect("FBD template serialises")
}

fn template_for_sfc(name: &str, type_: PouType) -> String {
    use crate::ld::{LdPouType, LdVarSection, LdVariable};
    use crate::sfc::{SfcAction, SfcProgram, SfcQualifier, SfcStep, SfcTransition};
    let pou_type = match type_ {
        PouType::Program => LdPouType::Program,
        PouType::FunctionBlock => LdPouType::FunctionBlock,
        PouType::Function => LdPouType::Program,
    };
    // Seed with the canonical "idle → running → idle" loop driven by
    // a `start` input. New users see the shape of an SFC POU at a
    // glance (steps + transitions + actions), and `cs check` returns
    // clean immediately.
    let prog = SfcProgram {
        name: name.into(),
        pou_type,
        variables: vec![
            LdVariable {
                name: "start".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Input,
                init: None,
            },
            LdVariable {
                name: "running_lamp".into(),
                type_name: "BOOL".into(),
                section: LdVarSection::Output,
                init: Some("FALSE".into()),
            },
        ],
        initial_step: "idle".into(),
        steps: vec![
            SfcStep {
                name: "idle".into(),
                actions: vec![SfcAction {
                    qualifier: SfcQualifier::R,
                    body: "running_lamp := FALSE".into(),
                }],
            },
            SfcStep {
                name: "running".into(),
                actions: vec![SfcAction {
                    qualifier: SfcQualifier::N,
                    body: "running_lamp := TRUE".into(),
                }],
            },
        ],
        transitions: vec![
            SfcTransition {
                from: "idle".into(),
                to: "running".into(),
                condition: "start".into(),
            },
            SfcTransition {
                from: "running".into(),
                to: "idle".into(),
                condition: "NOT start".into(),
            },
        ],
    };
    serde_json::to_string_pretty(&prog).expect("SFC template serialises")
}

fn default_config_for(protocol: Protocol) -> ProtocolConfig {
    match protocol {
        // New devices default to Modbus TCP on the demo slave port —
        // that's what `cs set devices/<n> --protocol modbus` lands a
        // user on. RTU users do `cs set devices/<n> --from rtu.json`
        // to swap the transport variant after creation.
        Protocol::Modbus => ProtocolConfig::Modbus(ModbusConfig {
            transport: crate::types::ModbusTransport::Tcp(crate::types::ModbusTcpParams {
                host: "127.0.0.1".into(),
                port: 502,
            }),
            slave_id: 1,
            poll_interval_ms: 100,
            timeout_ms: None,
            reconnect_backoff_ms: None,
            channels: vec![],
        }),
        Protocol::Ethercat => ProtocolConfig::Ethercat(crate::types::EthercatConfig {
            nic: default_ethercat_nic().into(),
            bringup: crate::types::EthercatBringup::Auto,
            cycle_us: 1_000,
            dc_sync: crate::types::EthercatDcSync::Off,
            dc_static_sync_iterations: 0,
            slaves: vec![],
            channels: vec![],
            gear: vec![],
        }),
        Protocol::Opcua => ProtocolConfig::Opcua(crate::types::OpcuaConfig {
            endpoint_url: "opc.tcp://127.0.0.1:4840".into(),
            auth: crate::types::OpcuaAuth::Anonymous,
            poll_interval_ms: 500,
            channels: vec![],
        }),
        // `_sim` mirrors the EtherCAT default: a fresh CANopen device
        // runs against the in-memory bus until ops point it at a real
        // SocketCAN interface on the edge.
        Protocol::Canopen => ProtocolConfig::Canopen(crate::types::CanopenConfig {
            interface: "_sim".into(),
            node_id: 1,
            bitrate: None,
            poll_interval_ms: 100,
            heartbeat_timeout_ms: 3000,
            start_on_connect: true,
            channels: vec![],
        }),
    }
}

#[cfg(test)]
mod ssh_target_tests {
    use super::*;

    fn store(dir: &std::path::Path) -> ProjectStore {
        ProjectStore::create(dir.to_path_buf(), "p").expect("create project")
    }

    /// The vector: `ssh` parses any destination starting with '-' as an
    /// option. `-oProxyCommand=<cmd>` runs `<cmd>` on the DEV machine at the
    /// next probe / deploy / attach. Confirmed against OpenSSH 10.3 before
    /// this guard existed.
    #[test]
    fn an_option_shaped_host_is_refused_by_every_door() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let evil = "-oProxyCommand=touch /tmp/pwned";

        let err = s.create_edge("bench", evil).unwrap_err();
        assert!(format!("{err}").contains("starts with '-'"), "{err}");

        // The PUT path and the read path are separate doors; both must shut,
        // because edges/*.toml also travels with a shared project.
        let mut edge = s.create_edge("ok", "grey0").unwrap();
        edge.host = evil.into();
        assert!(s.write_edge(&edge).is_err(), "write_edge must refuse");

        std::fs::write(
            dir.path().join("edges/handwritten.toml"),
            format!("host = \"{evil}\"\n"),
        )
        .unwrap();
        let err = s.read_edge("handwritten").unwrap_err();
        assert!(
            format!("{err}").contains("ProxyCommand"),
            "the why is named: {err}"
        );
    }

    #[test]
    fn an_option_shaped_ssh_user_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut edge = s.create_edge("bench", "grey0").unwrap();
        // `user@host` is one argument, so a leading '-' on the user half
        // makes the whole destination option-shaped.
        edge.ssh_user = "-oProxyCommand=id".into();
        let err = s.write_edge(&edge).unwrap_err();
        assert!(format!("{err}").contains("ssh_user"), "{err}");
    }

    #[test]
    fn whitespace_and_control_characters_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        for bad in ["grey0 -oProxyCommand=id", "grey0\nHost evil", "grey0\t"] {
            assert!(s.create_edge("x", bad).is_err(), "must refuse {bad:?}");
        }
    }

    /// Ordinary destinations keep working — this guard must not be the
    /// reason someone cannot reach their bench.
    #[test]
    fn ordinary_hosts_aliases_and_ipv6_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        for (i, good) in [
            "grey0",
            "192.168.1.50",
            "edge-01.plant.local",
            "2001:db8::1",
            "bench_rig",
        ]
        .iter()
        .enumerate()
        {
            let mut e = s.create_edge(&format!("e{i}"), good).expect(good);
            e.ssh_user = "operator".into();
            s.write_edge(&e).expect(good);
            assert_eq!(s.read_edge(&format!("e{i}")).expect(good).host, *good);
        }
        // An empty ssh_user means "let ssh decide" and stays legal.
        let mut e = s.create_edge("anon", "grey0").unwrap();
        e.ssh_user = String::new();
        s.write_edge(&e).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_store_refuses_reserved_ids_on_read_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::create(dir.path().join("project"), "test").unwrap();
        let text = "[[alarms]]\nid = '__device/bus0'\nvariable = 'x'\ncondition = 'is_true'\nmessage = 'shadow'\n";
        let config = toml::from_str(text).unwrap();
        assert!(store
            .write_alarms(&config)
            .unwrap_err()
            .to_string()
            .contains("reserved"));
        fs::write(store.root.join("alarms.toml"), text).unwrap();
        assert!(store
            .read_alarms()
            .unwrap_err()
            .to_string()
            .contains("reserved"));
    }

    // ---- crash-safe saves ----------------------------------------------

    const TORN_ROOT_ENV: &str = "IA2_TORN_WRITE_ROOT";

    fn iomap_with(n: usize) -> IoMap {
        let mut text = String::new();
        for i in 0..n {
            text.push_str(&format!(
                "[[mappings]]\napplication = \"main\"\nvariable = \"out_{i:03}\"\n\
                 direction = \"output\"\ndevice = \"plc_io\"\nchannel = \"do_{i:03}\"\n\n"
            ));
        }
        toml::from_str(&text).unwrap()
    }

    const TORN_FILE_ENV: &str = "IA2_TORN_WRITE_FILE";

    /// The child half of the test below: overwrite one file with contents
    /// larger than the file-size limit the parent imposed, so the kernel
    /// stops the process partway through the write.
    fn torn_write_child(root: &Path, which: &str) {
        let store = ProjectStore::open(root.to_path_buf()).unwrap();
        let result = match which {
            "pou" => store.write_pou_source(
                "main",
                &format!(
                    "PROGRAM main\n{}END_PROGRAM\n",
                    "  (* a long program *)\n".repeat(4_000)
                ),
            ),
            "iomap" => store.write_iomap(&iomap_with(2_000)),
            other => panic!("unknown torn-write target {other}"),
        };
        println!("WRITE_RETURNED {result:?}");
    }

    /// A save that dies partway — process killed, disk full, power lost —
    /// must leave the previous version. `fs::write` truncates first, so by
    /// the time it fails the previous version is already gone; for TOML, a
    /// cut that lands on a table boundary even parses cleanly, as a file
    /// with fewer entries than were saved.
    ///
    /// The failure is real, not simulated: the save runs in a child process
    /// under `ulimit -f`, and the kernel stops it (SIGXFSZ, or EFBIG where
    /// that is ignored) once the file it is writing reaches the limit.
    #[cfg(unix)]
    #[test]
    fn a_save_that_dies_partway_leaves_the_previous_version() {
        if let Some(root) = std::env::var_os(TORN_ROOT_ENV) {
            let which = std::env::var(TORN_FILE_ENV).unwrap();
            torn_write_child(Path::new(&root), &which);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        let store = ProjectStore::create(root.clone(), "p").unwrap();
        let original_source = store.read_pou_source("main").unwrap();
        store.write_iomap(&iomap_with(12)).unwrap();

        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for which in ["pou", "iomap"] {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                // A 32-block ceiling (16 or 32 KiB depending on the shell's
                // block size) — far below either write.
                .arg(
                    "ulimit -f 32 && exec \"$0\" --exact --nocapture --test-threads=1 \
                     store::tests::a_save_that_dies_partway_leaves_the_previous_version",
                )
                .arg(&exe)
                .env(TORN_ROOT_ENV, &root)
                .env(TORN_FILE_ENV, which)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            // The injection must actually have happened: the child was
            // killed, or its write reported the failure.
            assert!(
                !out.status.success() || stdout.contains("WRITE_RETURNED Err"),
                "the size limit did not stop the {which} write: {:?}\n{stdout}",
                out.status
            );
            children.push(format!("{which}: {:?}", out.status));
        }

        let pous_before = vec!["main".to_string()];
        let source = store.read_pou_source("main").unwrap();
        let iomap = store
            .read_iomap()
            .map(|m| m.mappings.len())
            .map_err(|e| e.to_string().lines().next().unwrap_or_default().to_string());
        let source_report = if source == original_source {
            "intact".to_string()
        } else {
            format!(
                "{} bytes instead of the original {}, ending {:?}",
                source.len(),
                original_source.len(),
                &source[source.len().saturating_sub(24)..]
            )
        };
        assert!(
            source == original_source && matches!(iomap, Ok(12)),
            "a save that did not complete damaged the project —
               pous/main.st: {source_report}
               iomap.toml read back as: {iomap:?} (12 mappings were saved)
               children: {children:?}"
        );
        // A killed save can leave its temporary behind; it is a dot-file,
        // and the project must not pick it up as a POU.
        let leftovers: Vec<_> = fs::read_dir(root.join("pous"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            !leftovers.is_empty(),
            "expected the killed save's temporary to remain"
        );
        assert_eq!(store.list_pou_paths().unwrap(), pous_before);
        ProjectStore::open(root.clone()).expect("project still opens");
    }

    #[test]
    fn read_hmi_distinguishes_absent_from_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::create(dir.path().join("p"), "p").unwrap();
        assert!(matches!(
            store.read_hmi("ghost"),
            Err(StoreError::HmiNotFound(_))
        ));
        store.create_hmi("s", "S").unwrap();
        assert!(store.read_hmi("s").is_ok());
        // A present-but-unparseable screen must never read as absent —
        // the generate guard's 409 fence depends on the distinction.
        fs::write(dir.path().join("p/hmi/s.hmi.json"), "{ not json").unwrap();
        assert!(matches!(
            store.read_hmi("s"),
            Err(StoreError::HmiCorrupt(..))
        ));
    }

    // ---- governance load validation ------------------------------------

    use crate::types::WriteMode;

    /// Write `body` as the project's manifest, bypassing the typed
    /// serializer — these tests exercise exactly the "operator edited
    /// project.toml by hand" path.
    fn write_manifest(root: &Path, body: &str) {
        fs::write(root.join("project.toml"), body).unwrap();
    }

    #[test]
    fn open_rejects_governance_with_swapped_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        ProjectStore::create(root.clone(), "p").unwrap();
        write_manifest(
            &root,
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
             [[governance.rules]]\nvariable = \"sp\"\nmin = 100.0\nmax = 0.0\n",
        );
        let err = ProjectStore::open(root)
            .err()
            .expect("open must reject the manifest")
            .to_string();
        assert!(err.contains("[governance] is invalid"), "{err}");
        assert!(err.contains("rule #1 ('sp')"), "{err}");
        assert!(err.contains("min (100) is greater than max (0)"), "{err}");
    }

    #[test]
    fn open_rejects_governance_with_non_finite_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        ProjectStore::create(root.clone(), "p").unwrap();
        write_manifest(
            &root,
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
             [[governance.rules]]\nvariable = \"sp\"\nmin = nan\n",
        );
        let err = ProjectStore::open(root)
            .err()
            .expect("open must reject the manifest")
            .to_string();
        assert!(err.contains("min must be a finite number"), "{err}");
    }

    #[test]
    fn open_rejects_governance_with_typoed_key() {
        // deny_unknown_fields makes this a plain TOML parse error.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        ProjectStore::create(root.clone(), "p").unwrap();
        write_manifest(
            &root,
            "name = \"p\"\n\n[governance]\nwritemode = \"allowlist\"\n",
        );
        let err = ProjectStore::open(root)
            .err()
            .expect("open must reject the manifest")
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
        assert!(err.contains("writemode"), "{err}");
    }

    #[test]
    fn open_accepts_valid_governance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        ProjectStore::create(root.clone(), "p").unwrap();
        write_manifest(
            &root,
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
             [[governance.rules]]\nvariable = \"sp\"\nmin = 0.0\nmax = 100.0\n",
        );
        let store = ProjectStore::open(root).unwrap();
        assert_eq!(store.governance().write_mode, WriteMode::Allowlist);
        assert_eq!(store.governance().rules.len(), 1);
    }

    #[test]
    fn read_governance_follows_disk_edits_where_the_cached_copy_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        let store = ProjectStore::create(root.clone(), "p").unwrap();
        assert!(store.governance().is_default());
        write_manifest(
            &root,
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
             [[governance.rules]]\nvariable = \"sp\"\nmax = 10.0\n",
        );
        // Fresh read sees the tightened policy…
        let fresh = store.read_governance().unwrap();
        assert_eq!(fresh.write_mode, WriteMode::Allowlist);
        assert_eq!(fresh.rules[0].max, Some(10.0));
        // …while the cached copy documents why per-run callers must not
        // rely on it.
        assert!(store.governance().is_default());
    }

    #[test]
    fn read_governance_rejects_an_invalid_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        let store = ProjectStore::create(root.clone(), "p").unwrap();
        write_manifest(
            &root,
            "name = \"p\"\n\n[governance]\nwrite_mode = \"allowlist\"\n\n\
             [[governance.rules]]\nvariable = \"\"\n",
        );
        let err = store.read_governance().unwrap_err().to_string();
        assert!(err.contains("[governance] is invalid"), "{err}");
        assert!(err.contains("variable must not be empty"), "{err}");
    }
}
