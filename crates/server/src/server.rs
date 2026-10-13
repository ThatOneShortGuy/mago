//! The single-workspace [`Server`]: the transport-agnostic core that owns one
//! workspace's file database, analysis service, and per-file derived-data
//! caches, and answers editor queries against them.

use std::sync::Arc;

use foldhash::HashMap;
use xxhash_rust::xxh3;

use mago_analyzer::analysis_result::AnalysisResult;
use mago_analyzer::artifacts::editor::EditorFileData;
use mago_codex::metadata::CodebaseMetadata;
use mago_codex::reference::SymbolReferences;
use mago_codex::ttype::union::TUnion;
use mago_database::Database;
use mago_database::DatabaseReader;
use mago_database::file::FileId;
use mago_database::file::FileType;
use mago_formatter::settings::FormatSettings;
use mago_orchestrator::service::incremental_analysis::IncrementalAnalysisService;
use mago_php_version::PHPVersion;
use mago_reporting::IssueCollection;

use crate::error::ServerError;
use crate::file_analysis;
use crate::file_analysis::FileAnalysis;
use crate::linter::LinterContext;
use crate::settings::Settings;

/// Return types inferred from function-like bodies, keyed by the declaration's
/// `(start, end)` byte span.
pub type InferredReturns = HashMap<(u32, u32), TUnion>;

/// A transport-agnostic backend for a single workspace.
///
/// Owns the workspace's [`Database`], an [`IncrementalAnalysisService`], a
/// [`LinterContext`], and the per-file analysis caches. It
/// performs no I/O: callers supply file contents as in-memory buffers and the
/// server answers queries in terms of [`FileId`]s and byte offsets.
pub struct Server {
    database: Database<'static>,
    service: IncrementalAnalysisService,
    linter: LinterContext,
    pub(crate) php_version: PHPVersion,
    pub(crate) formatter: FormatSettings,
    analyzer_enabled: bool,
    /// Per-file parse + resolve + lint results, keyed by content hash.
    file_analyses: HashMap<FileId, (u64, Arc<FileAnalysis>)>,
    /// Per-file return types inferred from function-like bodies, keyed by
    /// content hash. Observational only: the analyzer never consults these, but
    /// the editor offers them as a `@return` for a human to accept.
    inferred_returns_cache: HashMap<FileId, (u64, InferredReturns)>,
}

impl Server {
    /// Build a server for one workspace from an already-loaded file database,
    /// decoded codebase metadata, and resolved [`Settings`].
    ///
    /// Construction performs no analysis; call [`Server::analyze`] for the
    /// initial pass.
    #[must_use]
    pub fn new(
        database: Database<'static>,
        metadata: CodebaseMetadata,
        symbol_references: SymbolReferences,
        settings: Settings,
    ) -> Self {
        let Settings { php_version, features, parser, analyzer, linter, formatter, plugin_registry } = settings;

        let mut service = IncrementalAnalysisService::new(
            database.read_only(),
            metadata,
            symbol_references,
            analyzer,
            parser,
            plugin_registry,
        );
        service.set_editor_artifacts(true);

        let linter = LinterContext::new(linter, parser);

        Self {
            database,
            service,
            linter,
            php_version,
            formatter,
            analyzer_enabled: features.analyzer,
            file_analyses: HashMap::default(),
            inferred_returns_cache: HashMap::default(),
        }
    }

    /// Borrow the workspace file database.
    #[must_use]
    pub fn database(&self) -> &Database<'static> {
        &self.database
    }

    /// Mutably borrow the workspace file database (to add / update / delete
    /// files as the editor's buffers change).
    pub fn database_mut(&mut self) -> &mut Database<'static> {
        &mut self.database
    }

    /// Borrow the populated codebase metadata (symbol table).
    #[must_use]
    pub fn codebase(&self) -> &CodebaseMetadata {
        self.service.codebase()
    }

    /// The issues from the most recent analysis pass, if any.
    #[must_use]
    pub fn last_issues(&self) -> Option<IssueCollection> {
        self.service.last_issues()
    }

    /// Run a full analysis pass over the workspace. Returns an empty result
    /// when the analyzer is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the underlying analysis fails.
    pub fn analyze(&mut self) -> Result<AnalysisResult, ServerError> {
        if self.analyzer_enabled {
            Ok(self.service.analyze()?)
        } else {
            Ok(AnalysisResult::new(SymbolReferences::new()))
        }
    }

    /// Refresh the analysis service's view of the database, then re-analyze the
    /// `changed` files. Returns an empty result when the analyzer is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the underlying analysis fails.
    pub fn analyze_incremental(&mut self, changed: &[FileId]) -> Result<AnalysisResult, ServerError> {
        self.service.update_database(self.database.read_only());
        if self.analyzer_enabled {
            Ok(self.service.analyze_incremental(Some(changed))?)
        } else {
            Ok(AnalysisResult::new(SymbolReferences::new()))
        }
    }

    /// The analyzer's editor data (variable types, expression classes, member
    /// targets) for `file_id`, as of the most recent analysis pass. Returns
    /// `None` when the analyzer is disabled or the file has not been analyzed.
    #[must_use]
    pub fn editor_data_for(&self, file_id: FileId) -> Option<Arc<EditorFileData>> {
        if !self.analyzer_enabled {
            return None;
        }

        self.service.editor_file(file_id).cloned()
    }

    /// Return types inferred from each function-like body in `file_id`, keyed
    /// by the declaration's `(start, end)` byte span. Built on demand and
    /// cached per content hash. Returns `None` when the analyzer is disabled.
    pub fn inferred_returns_for(&mut self, file_id: FileId) -> Option<&InferredReturns> {
        if !self.analyzer_enabled {
            return None;
        }

        let file = self.database.get(&file_id).ok()?;
        let hash = xxh3::xxh3_64(&file.contents);

        if let Some((cached_hash, _)) = self.inferred_returns_cache.get(&file_id)
            && *cached_hash == hash
        {
            return self.inferred_returns_cache.get(&file_id).map(|(_, returns)| returns);
        }

        let (_, artifacts) = self.service.analyze_file_with_artifacts(file_id)?;
        let returns =
            artifacts.inferred_return_types_by_function_like.iter().map(|(span, ty)| (*span, (**ty).clone())).collect();
        self.inferred_returns_cache.insert(file_id, (hash, returns));
        self.inferred_returns_cache.get(&file_id).map(|(_, returns)| returns)
    }

    /// Drop cached derived data for the given files.
    pub fn invalidate_artifacts(&mut self, files: &[FileId]) {
        for id in files {
            self.inferred_returns_cache.remove(id);
            self.file_analyses.remove(id);
        }
    }

    /// Return the [`FileAnalysis`] for `file_id`, building it on a cache miss.
    /// One parse + resolve per content hash, shared across capability queries.
    pub fn file_analysis_for(&mut self, file_id: FileId) -> Option<Arc<FileAnalysis>> {
        let file = self.database.get(&file_id).ok()?;
        let hash = xxh3::xxh3_64(&file.contents);

        if let Some((cached_hash, analysis)) = self.file_analyses.get(&file_id)
            && *cached_hash == hash
        {
            return Some(Arc::clone(analysis));
        }

        let with_semantics = !self.analyzer_enabled;
        let analysis = Arc::new(file_analysis::build(&file, &self.linter, with_semantics));
        self.file_analyses.insert(file_id, (hash, Arc::clone(&analysis)));
        Some(analysis)
    }

    /// Build (or rebuild) the analysis for every changed host file.
    pub fn refresh_analyses(&mut self, file_ids: &[FileId]) {
        let with_semantics = !self.analyzer_enabled;
        for &file_id in file_ids {
            let Ok(file) = self.database.get(&file_id) else {
                self.file_analyses.remove(&file_id);
                continue;
            };

            if file.file_type != FileType::Host {
                continue;
            }

            let hash = xxh3::xxh3_64(&file.contents);
            let analysis = Arc::new(file_analysis::build(&file, &self.linter, with_semantics));
            self.file_analyses.insert(file_id, (hash, analysis));
        }
    }

    /// Build analyses for every host file in the database.
    pub fn refresh_all_host_analyses(&mut self) {
        let host_ids: Vec<FileId> =
            self.database.files().filter(|f| matches!(f.file_type, FileType::Host)).map(|f| f.id).collect();

        self.refresh_analyses(&host_ids);
    }

    /// Iterate the cached lint issues across every analyzed file.
    pub fn lint_issues(&self) -> impl Iterator<Item = &IssueCollection> + '_ {
        self.file_analyses.values().map(|(_, analysis)| &analysis.lint_issues)
    }

    /// Iterate every cached per-file analysis.
    pub fn analyses(&self) -> impl Iterator<Item = &Arc<FileAnalysis>> + '_ {
        self.file_analyses.values().map(|(_, analysis)| analysis)
    }
}
