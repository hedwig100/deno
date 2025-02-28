// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use super::analysis;
use super::cache;
use super::client::Client;
use super::config::ConfigSnapshot;
use super::documents;
use super::documents::Document;
use super::documents::DocumentsFilter;
use super::language_server;
use super::language_server::StateSnapshot;
use super::performance::Performance;
use super::tsc;
use super::tsc::TsServer;
use super::urls::LspClientUrl;
use super::urls::LspUrlMap;

use crate::args::LintOptions;
use crate::graph_util;
use crate::graph_util::enhanced_resolution_error_message;
use crate::lsp::lsp_custom::DiagnosticBatchNotificationParams;
use crate::resolver::SloppyImportsResolution;
use crate::resolver::SloppyImportsResolver;
use crate::tools::lint::get_configured_rules;

use deno_ast::MediaType;
use deno_core::anyhow::anyhow;
use deno_core::error::AnyError;
use deno_core::parking_lot::RwLock;
use deno_core::resolve_url;
use deno_core::serde::Deserialize;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::unsync::spawn;
use deno_core::unsync::spawn_blocking;
use deno_core::unsync::JoinHandle;
use deno_core::ModuleSpecifier;
use deno_graph::Resolution;
use deno_graph::ResolutionError;
use deno_graph::SpecifierError;
use deno_lint::rules::LintRule;
use deno_runtime::deno_fs;
use deno_runtime::deno_node;
use deno_runtime::tokio_util::create_basic_runtime;
use deno_semver::npm::NpmPackageReqReference;
use deno_semver::package::PackageReq;
use log::error;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tower_lsp::lsp_types as lsp;

#[derive(Debug)]
pub struct DiagnosticServerUpdateMessage {
  pub snapshot: Arc<StateSnapshot>,
  pub config: Arc<ConfigSnapshot>,
  pub lint_options: LintOptions,
  pub url_map: LspUrlMap,
}

struct DiagnosticRecord {
  pub specifier: ModuleSpecifier,
  pub versioned: VersionedDiagnostics,
}

#[derive(Clone, Default, Debug)]
struct VersionedDiagnostics {
  pub version: Option<i32>,
  pub diagnostics: Vec<lsp::Diagnostic>,
}

type DiagnosticVec = Vec<DiagnosticRecord>;

#[derive(Debug, Hash, PartialEq, Eq, Copy, Clone)]
pub enum DiagnosticSource {
  Deno,
  Lint,
  Ts,
}

impl DiagnosticSource {
  pub fn as_lsp_source(&self) -> &'static str {
    match self {
      Self::Deno => "deno",
      Self::Lint => "deno-lint",
      Self::Ts => "deno-ts",
    }
  }
}

type DiagnosticsBySource = HashMap<DiagnosticSource, VersionedDiagnostics>;

#[derive(Debug)]
struct DiagnosticsPublisher {
  client: Client,
  state: Arc<DiagnosticsState>,
  diagnostics_by_specifier:
    Mutex<HashMap<ModuleSpecifier, DiagnosticsBySource>>,
}

impl DiagnosticsPublisher {
  pub fn new(client: Client, state: Arc<DiagnosticsState>) -> Self {
    Self {
      client,
      state,
      diagnostics_by_specifier: Default::default(),
    }
  }

  pub async fn publish(
    &self,
    source: DiagnosticSource,
    diagnostics: DiagnosticVec,
    url_map: &LspUrlMap,
    token: &CancellationToken,
  ) -> usize {
    let mut diagnostics_by_specifier =
      self.diagnostics_by_specifier.lock().await;
    let mut seen_specifiers = HashSet::with_capacity(diagnostics.len());
    let mut messages_sent = 0;

    for record in diagnostics {
      if token.is_cancelled() {
        return messages_sent;
      }

      seen_specifiers.insert(record.specifier.clone());

      let diagnostics_by_source = diagnostics_by_specifier
        .entry(record.specifier.clone())
        .or_default();
      let version = record.versioned.version;
      let source_diagnostics = diagnostics_by_source.entry(source).or_default();
      *source_diagnostics = record.versioned;

      // DO NOT filter these by version. We want to display even out
      // of date diagnostics in order to prevent flickering. The user's
      // lsp client will eventually catch up.
      let all_specifier_diagnostics = diagnostics_by_source
        .values()
        .flat_map(|d| &d.diagnostics)
        .cloned()
        .collect::<Vec<_>>();

      self
        .state
        .update(&record.specifier, version, &all_specifier_diagnostics);
      self
        .client
        .when_outside_lsp_lock()
        .publish_diagnostics(
          url_map
            .normalize_specifier(&record.specifier)
            .unwrap_or(LspClientUrl::new(record.specifier)),
          all_specifier_diagnostics,
          version,
        )
        .await;
      messages_sent += 1;
    }

    // now check all the specifiers to clean up any ones with old diagnostics
    let mut specifiers_to_remove = Vec::new();
    for (specifier, diagnostics_by_source) in
      diagnostics_by_specifier.iter_mut()
    {
      if seen_specifiers.contains(specifier) {
        continue;
      }
      if token.is_cancelled() {
        break;
      }
      let maybe_removed_value = diagnostics_by_source.remove(&source);
      if diagnostics_by_source.is_empty() {
        specifiers_to_remove.push(specifier.clone());
        if let Some(removed_value) = maybe_removed_value {
          // clear out any diagnostics for this specifier
          self.state.update(specifier, removed_value.version, &[]);
          self
            .client
            .when_outside_lsp_lock()
            .publish_diagnostics(
              url_map
                .normalize_specifier(specifier)
                .unwrap_or_else(|_| LspClientUrl::new(specifier.clone())),
              Vec::new(),
              removed_value.version,
            )
            .await;
          messages_sent += 1;
        }
      }
    }

    // clean up specifiers with no diagnostics
    for specifier in specifiers_to_remove {
      diagnostics_by_specifier.remove(&specifier);
    }

    messages_sent
  }

  pub async fn clear(&self) {
    let mut all_diagnostics = self.diagnostics_by_specifier.lock().await;
    all_diagnostics.clear();
  }
}

type DiagnosticMap = HashMap<ModuleSpecifier, VersionedDiagnostics>;

#[derive(Clone, Default, Debug)]
struct TsDiagnosticsStore(Arc<deno_core::parking_lot::Mutex<DiagnosticMap>>);

impl TsDiagnosticsStore {
  pub fn get(
    &self,
    specifier: &ModuleSpecifier,
    document_version: Option<i32>,
  ) -> Vec<lsp::Diagnostic> {
    let ts_diagnostics = self.0.lock();
    if let Some(versioned) = ts_diagnostics.get(specifier) {
      // only get the diagnostics if they're up to date
      if document_version == versioned.version {
        return versioned.diagnostics.clone();
      }
    }
    Vec::new()
  }

  pub fn invalidate(&self, specifiers: &[ModuleSpecifier]) {
    let mut ts_diagnostics = self.0.lock();
    for specifier in specifiers {
      ts_diagnostics.remove(specifier);
    }
  }

  pub fn invalidate_all(&self) {
    self.0.lock().clear();
  }

  fn update(&self, diagnostics: &DiagnosticVec) {
    let mut stored_ts_diagnostics = self.0.lock();
    *stored_ts_diagnostics = diagnostics
      .iter()
      .map(|record| (record.specifier.clone(), record.versioned.clone()))
      .collect();
  }
}

pub fn should_send_diagnostic_batch_index_notifications() -> bool {
  crate::args::has_flag_env_var(
    "DENO_DONT_USE_INTERNAL_LSP_DIAGNOSTIC_SYNC_FLAG",
  )
}

#[derive(Clone, Debug)]
struct DiagnosticBatchCounter(Option<Arc<AtomicUsize>>);

impl Default for DiagnosticBatchCounter {
  fn default() -> Self {
    if should_send_diagnostic_batch_index_notifications() {
      Self(Some(Default::default()))
    } else {
      Self(None)
    }
  }
}

impl DiagnosticBatchCounter {
  pub fn inc(&self) -> Option<usize> {
    self
      .0
      .as_ref()
      .map(|value| value.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
  }

  pub fn get(&self) -> Option<usize> {
    self
      .0
      .as_ref()
      .map(|value| value.load(std::sync::atomic::Ordering::SeqCst))
  }
}

#[derive(Debug)]
enum ChannelMessage {
  Update(ChannelUpdateMessage),
  Clear,
}

#[derive(Debug)]
struct ChannelUpdateMessage {
  message: DiagnosticServerUpdateMessage,
  batch_index: Option<usize>,
}

#[derive(Debug)]
struct SpecifierState {
  version: Option<i32>,
  no_cache_diagnostics: Vec<lsp::Diagnostic>,
}

#[derive(Debug, Default)]
pub struct DiagnosticsState {
  specifiers: RwLock<HashMap<ModuleSpecifier, SpecifierState>>,
}

impl DiagnosticsState {
  fn update(
    &self,
    specifier: &ModuleSpecifier,
    version: Option<i32>,
    diagnostics: &[lsp::Diagnostic],
  ) {
    let mut specifiers = self.specifiers.write();
    let current_version = specifiers.get(specifier).and_then(|s| s.version);
    match (version, current_version) {
      (Some(arg), Some(existing)) if arg < existing => return,
      _ => {}
    }
    let mut no_cache_diagnostics = vec![];
    for diagnostic in diagnostics {
      if diagnostic.code
        == Some(lsp::NumberOrString::String("no-cache".to_string()))
        || diagnostic.code
          == Some(lsp::NumberOrString::String("no-cache-npm".to_string()))
      {
        no_cache_diagnostics.push(diagnostic.clone());
      }
    }
    specifiers.insert(
      specifier.clone(),
      SpecifierState {
        version,
        no_cache_diagnostics,
      },
    );
  }

  pub fn clear(&self, specifier: &ModuleSpecifier) {
    self.specifiers.write().remove(specifier);
  }

  pub fn has_no_cache_diagnostics(&self, specifier: &ModuleSpecifier) -> bool {
    self
      .specifiers
      .read()
      .get(specifier)
      .map(|s| !s.no_cache_diagnostics.is_empty())
      .unwrap_or(false)
  }

  pub fn no_cache_diagnostics(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Vec<lsp::Diagnostic> {
    self
      .specifiers
      .read()
      .get(specifier)
      .map(|s| s.no_cache_diagnostics.clone())
      .unwrap_or_default()
  }
}

#[derive(Debug)]
pub struct DiagnosticsServer {
  channel: Option<mpsc::UnboundedSender<ChannelMessage>>,
  ts_diagnostics: TsDiagnosticsStore,
  client: Client,
  performance: Arc<Performance>,
  ts_server: Arc<TsServer>,
  batch_counter: DiagnosticBatchCounter,
  state: Arc<DiagnosticsState>,
}

impl DiagnosticsServer {
  pub fn new(
    client: Client,
    performance: Arc<Performance>,
    ts_server: Arc<TsServer>,
    state: Arc<DiagnosticsState>,
  ) -> Self {
    DiagnosticsServer {
      channel: Default::default(),
      ts_diagnostics: Default::default(),
      client,
      performance,
      ts_server,
      batch_counter: Default::default(),
      state,
    }
  }

  pub fn get_ts_diagnostics(
    &self,
    specifier: &ModuleSpecifier,
    document_version: Option<i32>,
  ) -> Vec<lsp::Diagnostic> {
    self.ts_diagnostics.get(specifier, document_version)
  }

  pub fn invalidate(&self, specifiers: &[ModuleSpecifier]) {
    self.ts_diagnostics.invalidate(specifiers);
  }

  pub fn invalidate_all(&self) {
    self.ts_diagnostics.invalidate_all();
    if let Some(tx) = &self.channel {
      let _ = tx.send(ChannelMessage::Clear);
    }
  }

  #[allow(unused_must_use)]
  pub fn start(&mut self) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChannelMessage>();
    self.channel = Some(tx);
    let client = self.client.clone();
    let state = self.state.clone();
    let performance = self.performance.clone();
    let ts_diagnostics_store = self.ts_diagnostics.clone();
    let ts_server = self.ts_server.clone();

    let _join_handle = thread::spawn(move || {
      let runtime = create_basic_runtime();

      runtime.block_on(async {
        let mut token = CancellationToken::new();
        let mut ts_handle: Option<JoinHandle<()>> = None;
        let mut lint_handle: Option<JoinHandle<()>> = None;
        let mut deps_handle: Option<JoinHandle<()>> = None;
        let diagnostics_publisher =
          Arc::new(DiagnosticsPublisher::new(client.clone(), state.clone()));

        loop {
          match rx.recv().await {
            // channel has closed
            None => break,
            Some(message) => {
              let message = match message {
                ChannelMessage::Update(message) => message,
                ChannelMessage::Clear => {
                  token.cancel();
                  token = CancellationToken::new();
                  diagnostics_publisher.clear().await;
                  continue;
                }
              };
              let ChannelUpdateMessage {
                message:
                  DiagnosticServerUpdateMessage {
                    snapshot,
                    config,
                    lint_options,
                    url_map,
                  },
                batch_index,
              } = message;
              let url_map = Arc::new(url_map);

              // cancel the previous run
              token.cancel();
              token = CancellationToken::new();

              let previous_ts_handle = ts_handle.take();
              ts_handle = Some(spawn({
                let performance = performance.clone();
                let diagnostics_publisher = diagnostics_publisher.clone();
                let ts_server = ts_server.clone();
                let token = token.clone();
                let ts_diagnostics_store = ts_diagnostics_store.clone();
                let snapshot = snapshot.clone();
                let config = config.clone();
                let url_map = url_map.clone();
                async move {
                  if let Some(previous_handle) = previous_ts_handle {
                    // Wait on the previous run to complete in order to prevent
                    // multiple threads queueing up a lot of tsc requests.
                    // Do not race this with cancellation because we want a
                    // chain of events to wait for all the previous diagnostics to complete
                    previous_handle.await;
                  }

                  // Debounce timer delay. 150ms between keystrokes is about 45 WPM, so we
                  // want something that is longer than that, but not too long to
                  // introduce detectable UI delay; 200ms is a decent compromise.
                  const DELAY: Duration = Duration::from_millis(200);
                  tokio::select! {
                    _ = token.cancelled() => { return; }
                    _ = tokio::time::sleep(DELAY) => {}
                  };

                  let mark = performance.mark("lsp.update_diagnostics_ts");
                  let diagnostics = generate_ts_diagnostics(
                    snapshot.clone(),
                    &config,
                    &ts_server,
                    token.clone(),
                  )
                  .await
                  .map_err(|err| {
                    if !token.is_cancelled() {
                      error!(
                        "Error generating TypeScript diagnostics: {}",
                        err
                      );
                    }
                  })
                  .unwrap_or_default();

                  let mut messages_len = 0;
                  if !token.is_cancelled() {
                    ts_diagnostics_store.update(&diagnostics);
                    messages_len = diagnostics_publisher
                      .publish(
                        DiagnosticSource::Ts,
                        diagnostics,
                        &url_map,
                        &token,
                      )
                      .await;

                    if !token.is_cancelled() {
                      performance.measure(mark);
                    }
                  }

                  if let Some(batch_index) = batch_index {
                    diagnostics_publisher
                      .client
                      .send_diagnostic_batch_notification(
                        DiagnosticBatchNotificationParams {
                          batch_index,
                          messages_len,
                        },
                      );
                  }
                }
              }));

              let previous_deps_handle = deps_handle.take();
              deps_handle = Some(spawn({
                let performance = performance.clone();
                let diagnostics_publisher = diagnostics_publisher.clone();
                let token = token.clone();
                let snapshot = snapshot.clone();
                let config = config.clone();
                let url_map = url_map.clone();
                async move {
                  if let Some(previous_handle) = previous_deps_handle {
                    previous_handle.await;
                  }
                  let mark = performance.mark("lsp.update_diagnostics_deps");
                  let diagnostics = spawn_blocking({
                    let token = token.clone();
                    move || generate_deno_diagnostics(&snapshot, &config, token)
                  })
                  .await
                  .unwrap();

                  let mut messages_len = 0;
                  if !token.is_cancelled() {
                    messages_len = diagnostics_publisher
                      .publish(
                        DiagnosticSource::Deno,
                        diagnostics,
                        &url_map,
                        &token,
                      )
                      .await;

                    if !token.is_cancelled() {
                      performance.measure(mark);
                    }
                  }

                  if let Some(batch_index) = batch_index {
                    diagnostics_publisher
                      .client
                      .send_diagnostic_batch_notification(
                        DiagnosticBatchNotificationParams {
                          batch_index,
                          messages_len,
                        },
                      );
                  }
                }
              }));

              let previous_lint_handle = lint_handle.take();
              lint_handle = Some(spawn({
                let performance = performance.clone();
                let diagnostics_publisher = diagnostics_publisher.clone();
                let token = token.clone();
                let snapshot = snapshot.clone();
                let config = config.clone();
                let url_map = url_map.clone();
                async move {
                  if let Some(previous_handle) = previous_lint_handle {
                    previous_handle.await;
                  }
                  let mark = performance.mark("lsp.update_diagnostics_lint");
                  let diagnostics = spawn_blocking({
                    let token = token.clone();
                    move || {
                      generate_lint_diagnostics(
                        &snapshot,
                        &config,
                        &lint_options,
                        token,
                      )
                    }
                  })
                  .await
                  .unwrap();

                  let mut messages_len = 0;
                  if !token.is_cancelled() {
                    messages_len = diagnostics_publisher
                      .publish(
                        DiagnosticSource::Lint,
                        diagnostics,
                        &url_map,
                        &token,
                      )
                      .await;

                    if !token.is_cancelled() {
                      performance.measure(mark);
                    }
                  }

                  if let Some(batch_index) = batch_index {
                    diagnostics_publisher
                      .client
                      .send_diagnostic_batch_notification(
                        DiagnosticBatchNotificationParams {
                          batch_index,
                          messages_len,
                        },
                      );
                  }
                }
              }));
            }
          }
        }
      })
    });
  }

  pub fn latest_batch_index(&self) -> Option<usize> {
    self.batch_counter.get()
  }

  pub fn update(
    &self,
    message: DiagnosticServerUpdateMessage,
  ) -> Result<(), AnyError> {
    // todo(dsherret): instead of queuing up messages, it would be better to
    // instead only store the latest message (ex. maybe using a
    // tokio::sync::watch::channel)
    if let Some(tx) = &self.channel {
      tx.send(ChannelMessage::Update(ChannelUpdateMessage {
        message,
        batch_index: self.batch_counter.inc(),
      }))
      .map_err(|err| err.into())
    } else {
      Err(anyhow!("diagnostics server not started"))
    }
  }
}

impl<'a> From<&'a crate::tsc::DiagnosticCategory> for lsp::DiagnosticSeverity {
  fn from(category: &'a crate::tsc::DiagnosticCategory) -> Self {
    match category {
      crate::tsc::DiagnosticCategory::Error => lsp::DiagnosticSeverity::ERROR,
      crate::tsc::DiagnosticCategory::Warning => {
        lsp::DiagnosticSeverity::WARNING
      }
      crate::tsc::DiagnosticCategory::Suggestion => {
        lsp::DiagnosticSeverity::HINT
      }
      crate::tsc::DiagnosticCategory::Message => {
        lsp::DiagnosticSeverity::INFORMATION
      }
    }
  }
}

impl<'a> From<&'a crate::tsc::Position> for lsp::Position {
  fn from(pos: &'a crate::tsc::Position) -> Self {
    Self {
      line: pos.line as u32,
      character: pos.character as u32,
    }
  }
}

fn get_diagnostic_message(diagnostic: &crate::tsc::Diagnostic) -> String {
  if let Some(message) = diagnostic.message_text.clone() {
    message
  } else if let Some(message_chain) = diagnostic.message_chain.clone() {
    message_chain.format_message(0)
  } else {
    "[missing message]".to_string()
  }
}

fn to_lsp_range(
  start: &crate::tsc::Position,
  end: &crate::tsc::Position,
) -> lsp::Range {
  lsp::Range {
    start: start.into(),
    end: end.into(),
  }
}

fn to_lsp_related_information(
  related_information: &Option<Vec<crate::tsc::Diagnostic>>,
) -> Option<Vec<lsp::DiagnosticRelatedInformation>> {
  related_information.as_ref().map(|related| {
    related
      .iter()
      .filter_map(|ri| {
        if let (Some(file_name), Some(start), Some(end)) =
          (&ri.file_name, &ri.start, &ri.end)
        {
          let uri = lsp::Url::parse(file_name).unwrap();
          Some(lsp::DiagnosticRelatedInformation {
            location: lsp::Location {
              uri,
              range: to_lsp_range(start, end),
            },
            message: get_diagnostic_message(ri),
          })
        } else {
          None
        }
      })
      .collect()
  })
}

fn ts_json_to_diagnostics(
  diagnostics: Vec<crate::tsc::Diagnostic>,
) -> Vec<lsp::Diagnostic> {
  diagnostics
    .iter()
    .filter_map(|d| {
      if let (Some(start), Some(end)) = (&d.start, &d.end) {
        Some(lsp::Diagnostic {
          range: to_lsp_range(start, end),
          severity: Some((&d.category).into()),
          code: Some(lsp::NumberOrString::Number(d.code as i32)),
          code_description: None,
          source: Some(DiagnosticSource::Ts.as_lsp_source().to_string()),
          message: get_diagnostic_message(d),
          related_information: to_lsp_related_information(
            &d.related_information,
          ),
          tags: match d.code {
            // These are codes that indicate the variable is unused.
            2695 | 6133 | 6138 | 6192 | 6196 | 6198 | 6199 | 6205 | 7027
            | 7028 => Some(vec![lsp::DiagnosticTag::UNNECESSARY]),
            // These are codes that indicated the variable is deprecated.
            2789 | 6385 | 6387 => Some(vec![lsp::DiagnosticTag::DEPRECATED]),
            _ => None,
          },
          data: None,
        })
      } else {
        None
      }
    })
    .collect()
}

fn generate_lint_diagnostics(
  snapshot: &language_server::StateSnapshot,
  config: &ConfigSnapshot,
  lint_options: &LintOptions,
  token: CancellationToken,
) -> DiagnosticVec {
  let documents = snapshot
    .documents
    .documents(DocumentsFilter::OpenDiagnosable);
  let lint_rules = get_configured_rules(lint_options.rules.clone());
  let mut diagnostics_vec = Vec::new();
  for document in documents {
    let settings =
      config.workspace_settings_for_specifier(document.specifier());
    if !settings.lint {
      continue;
    }
    // exit early if cancelled
    if token.is_cancelled() {
      break;
    }
    // ignore any npm package files
    if let Some(npm) = &snapshot.npm {
      if npm.node_resolver.in_npm_package(document.specifier()) {
        continue;
      }
    }
    let version = document.maybe_lsp_version();
    diagnostics_vec.push(DiagnosticRecord {
      specifier: document.specifier().clone(),
      versioned: VersionedDiagnostics {
        version,
        diagnostics: generate_document_lint_diagnostics(
          config,
          lint_options,
          lint_rules.clone(),
          &document,
        ),
      },
    });
  }
  diagnostics_vec
}

fn generate_document_lint_diagnostics(
  config: &ConfigSnapshot,
  lint_options: &LintOptions,
  lint_rules: Vec<&'static dyn LintRule>,
  document: &Document,
) -> Vec<lsp::Diagnostic> {
  if !config.specifier_enabled(document.specifier()) {
    return Vec::new();
  }
  if !lint_options.files.matches_specifier(document.specifier()) {
    return Vec::new();
  }
  match document.maybe_parsed_source() {
    Some(Ok(parsed_source)) => {
      if let Ok(references) =
        analysis::get_lint_references(&parsed_source, lint_rules)
      {
        references
          .into_iter()
          .map(|r| r.to_diagnostic())
          .collect::<Vec<_>>()
      } else {
        Vec::new()
      }
    }
    Some(Err(_)) => Vec::new(),
    None => {
      error!("Missing file contents for: {}", document.specifier());
      Vec::new()
    }
  }
}

async fn generate_ts_diagnostics(
  snapshot: Arc<language_server::StateSnapshot>,
  config: &ConfigSnapshot,
  ts_server: &tsc::TsServer,
  token: CancellationToken,
) -> Result<DiagnosticVec, AnyError> {
  let mut diagnostics_vec = Vec::new();
  let specifiers = snapshot
    .documents
    .documents(DocumentsFilter::OpenDiagnosable)
    .into_iter()
    .map(|d| d.specifier().clone());
  let (enabled_specifiers, disabled_specifiers) = specifiers
    .into_iter()
    .partition::<Vec<_>, _>(|s| config.specifier_enabled(s));
  let ts_diagnostics_map = if !enabled_specifiers.is_empty() {
    ts_server
      .get_diagnostics(snapshot.clone(), enabled_specifiers, token)
      .await?
  } else {
    Default::default()
  };
  for (specifier_str, ts_json_diagnostics) in ts_diagnostics_map {
    let specifier = resolve_url(&specifier_str)?;
    let version = snapshot
      .documents
      .get(&specifier)
      .and_then(|d| d.maybe_lsp_version());
    // check if the specifier is enabled again just in case TS returns us
    // diagnostics for a disabled specifier
    let ts_diagnostics = if config.specifier_enabled(&specifier) {
      ts_json_to_diagnostics(ts_json_diagnostics)
    } else {
      Vec::new()
    };
    diagnostics_vec.push(DiagnosticRecord {
      specifier,
      versioned: VersionedDiagnostics {
        version,
        diagnostics: ts_diagnostics,
      },
    });
  }
  // add an empty diagnostic publish for disabled specifiers in order
  // to clear those diagnostics if they exist
  for specifier in disabled_specifiers {
    let version = snapshot
      .documents
      .get(&specifier)
      .and_then(|d| d.maybe_lsp_version());
    diagnostics_vec.push(DiagnosticRecord {
      specifier,
      versioned: VersionedDiagnostics {
        version,
        diagnostics: Vec::new(),
      },
    });
  }
  Ok(diagnostics_vec)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticDataSpecifier {
  pub specifier: ModuleSpecifier,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticDataStrSpecifier {
  pub specifier: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticDataRedirect {
  pub redirect: ModuleSpecifier,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticDataNoLocal {
  pub to: ModuleSpecifier,
  pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticDataImportMapRemap {
  pub from: String,
  pub to: String,
}

/// An enum which represents diagnostic errors which originate from Deno itself.
pub enum DenoDiagnostic {
  /// A `x-deno-warning` is associated with the specifier and should be displayed
  /// as a warning to the user.
  DenoWarn(String),
  /// An informational diagnostic that indicates an existing specifier can be
  /// remapped to an import map import specifier.
  ImportMapRemap { from: String, to: String },
  /// The import assertion type is incorrect.
  InvalidAttributeType(String),
  /// A module requires an attribute type to be a valid import.
  NoAttributeType,
  /// A remote module was not found in the cache.
  NoCache(ModuleSpecifier),
  /// A remote npm package reference was not found in the cache.
  NoCacheNpm(PackageReq, ModuleSpecifier),
  /// A local module was not found on the local file system.
  NoLocal(ModuleSpecifier),
  /// The specifier resolved to a remote specifier that was redirected to
  /// another specifier.
  Redirect {
    from: ModuleSpecifier,
    to: ModuleSpecifier,
  },
  /// An error occurred when resolving the specifier string.
  ResolutionError(deno_graph::ResolutionError),
  /// Invalid `node:` specifier.
  InvalidNodeSpecifier(ModuleSpecifier),
  /// Bare specifier is used for `node:` specifier
  BareNodeSpecifier(String),
}

impl DenoDiagnostic {
  fn code(&self) -> &str {
    match self {
      Self::DenoWarn(_) => "deno-warn",
      Self::ImportMapRemap { .. } => "import-map-remap",
      Self::InvalidAttributeType(_) => "invalid-attribute-type",
      Self::NoAttributeType => "no-attribute-type",
      Self::NoCache(_) => "no-cache",
      Self::NoCacheNpm(_, _) => "no-cache-npm",
      Self::NoLocal(_) => "no-local",
      Self::Redirect { .. } => "redirect",
      Self::ResolutionError(err) => {
        if graph_util::get_resolution_error_bare_node_specifier(err).is_some() {
          "import-node-prefix-missing"
        } else {
          match err {
            ResolutionError::InvalidDowngrade { .. } => "invalid-downgrade",
            ResolutionError::InvalidLocalImport { .. } => {
              "invalid-local-import"
            }
            ResolutionError::InvalidSpecifier { error, .. } => match error {
              SpecifierError::ImportPrefixMissing(_, _) => {
                "import-prefix-missing"
              }
              SpecifierError::InvalidUrl(_) => "invalid-url",
            },
            ResolutionError::ResolverError { .. } => "resolver-error",
          }
        }
      }
      Self::InvalidNodeSpecifier(_) => "resolver-error",
      Self::BareNodeSpecifier(_) => "import-node-prefix-missing",
    }
  }

  /// A "static" method which for a diagnostic that originated from the
  /// structure returns a code action which can resolve the diagnostic.
  pub fn get_code_action(
    specifier: &ModuleSpecifier,
    diagnostic: &lsp::Diagnostic,
  ) -> Result<lsp::CodeAction, AnyError> {
    if let Some(lsp::NumberOrString::String(code)) = &diagnostic.code {
      let code_action = match code.as_str() {
        "import-map-remap" => {
          let data = diagnostic
            .data
            .clone()
            .ok_or_else(|| anyhow!("Diagnostic is missing data"))?;
          let DiagnosticDataImportMapRemap { from, to } =
            serde_json::from_value(data)?;
          lsp::CodeAction {
            title: format!("Update \"{from}\" to \"{to}\" to use import map."),
            kind: Some(lsp::CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(lsp::WorkspaceEdit {
              changes: Some(HashMap::from([(
                specifier.clone(),
                vec![lsp::TextEdit {
                  new_text: format!("\"{to}\""),
                  range: diagnostic.range,
                }],
              )])),
              ..Default::default()
            }),
            ..Default::default()
          }
        }
        "no-attribute-type" => lsp::CodeAction {
          title: "Insert import attribute.".to_string(),
          kind: Some(lsp::CodeActionKind::QUICKFIX),
          diagnostics: Some(vec![diagnostic.clone()]),
          edit: Some(lsp::WorkspaceEdit {
            changes: Some(HashMap::from([(
              specifier.clone(),
              vec![lsp::TextEdit {
                new_text: " with { type: \"json\" }".to_string(),
                range: lsp::Range {
                  start: diagnostic.range.end,
                  end: diagnostic.range.end,
                },
              }],
            )])),
            ..Default::default()
          }),
          ..Default::default()
        },
        "no-cache" | "no-cache-npm" => {
          let data = diagnostic
            .data
            .clone()
            .ok_or_else(|| anyhow!("Diagnostic is missing data"))?;
          let data: DiagnosticDataSpecifier = serde_json::from_value(data)?;
          lsp::CodeAction {
            title: format!(
              "Cache \"{}\" and its dependencies.",
              data.specifier
            ),
            kind: Some(lsp::CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            command: Some(lsp::Command {
              title: "".to_string(),
              command: "deno.cache".to_string(),
              arguments: Some(vec![json!([data.specifier]), json!(&specifier)]),
            }),
            ..Default::default()
          }
        }
        "no-local" => {
          let data = diagnostic
            .data
            .clone()
            .ok_or_else(|| anyhow!("Diagnostic is missing data"))?;
          let data: DiagnosticDataNoLocal = serde_json::from_value(data)?;
          lsp::CodeAction {
            title: data.message,
            kind: Some(lsp::CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(lsp::WorkspaceEdit {
              changes: Some(HashMap::from([(
                specifier.clone(),
                vec![lsp::TextEdit {
                  new_text: format!(
                    "\"{}\"",
                    relative_specifier(&data.to, specifier)
                  ),
                  range: diagnostic.range,
                }],
              )])),
              ..Default::default()
            }),
            ..Default::default()
          }
        }
        "redirect" => {
          let data = diagnostic
            .data
            .clone()
            .ok_or_else(|| anyhow!("Diagnostic is missing data"))?;
          let data: DiagnosticDataRedirect = serde_json::from_value(data)?;
          lsp::CodeAction {
            title: "Update specifier to its redirected specifier.".to_string(),
            kind: Some(lsp::CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(lsp::WorkspaceEdit {
              changes: Some(HashMap::from([(
                specifier.clone(),
                vec![lsp::TextEdit {
                  new_text: format!(
                    "\"{}\"",
                    specifier_text_for_redirected(&data.redirect, specifier)
                  ),
                  range: diagnostic.range,
                }],
              )])),
              ..Default::default()
            }),
            ..Default::default()
          }
        }
        "import-node-prefix-missing" => {
          let data = diagnostic
            .data
            .clone()
            .ok_or_else(|| anyhow!("Diagnostic is missing data"))?;
          let data: DiagnosticDataStrSpecifier = serde_json::from_value(data)?;
          lsp::CodeAction {
            title: format!("Update specifier to node:{}", data.specifier),
            kind: Some(lsp::CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(lsp::WorkspaceEdit {
              changes: Some(HashMap::from([(
                specifier.clone(),
                vec![lsp::TextEdit {
                  new_text: format!("\"node:{}\"", data.specifier),
                  range: diagnostic.range,
                }],
              )])),
              ..Default::default()
            }),
            ..Default::default()
          }
        }
        _ => {
          return Err(anyhow!(
            "Unsupported diagnostic code (\"{}\") provided.",
            code
          ))
        }
      };
      Ok(code_action)
    } else {
      Err(anyhow!("Unsupported diagnostic code provided."))
    }
  }

  /// Given a reference to the code from an LSP diagnostic, determine if the
  /// diagnostic is fixable or not
  pub fn is_fixable(diagnostic: &lsp_types::Diagnostic) -> bool {
    if let Some(lsp::NumberOrString::String(code)) = &diagnostic.code {
      match code.as_str() {
        "import-map-remap"
        | "no-cache"
        | "no-cache-npm"
        | "no-attribute-type"
        | "redirect"
        | "import-node-prefix-missing" => true,
        "no-local" => diagnostic.data.is_some(),
        _ => false,
      }
    } else {
      false
    }
  }

  /// Convert to an lsp Diagnostic when the range the diagnostic applies to is
  /// provided.
  pub fn to_lsp_diagnostic(&self, range: &lsp::Range) -> lsp::Diagnostic {
    fn no_local_message(
      specifier: &ModuleSpecifier,
      sloppy_resolution: SloppyImportsResolution,
    ) -> String {
      let mut message =
        format!("Unable to load a local module: {}\n", specifier);
      if let Some(additional_message) =
        sloppy_resolution.as_suggestion_message()
      {
        message.push_str(&additional_message);
        message.push('.');
      } else {
        message.push_str("Please check the file path.");
      }
      message
    }

    let (severity, message, data) = match self {
      Self::DenoWarn(message) => (lsp::DiagnosticSeverity::WARNING, message.to_string(), None),
      Self::ImportMapRemap { from, to } => (lsp::DiagnosticSeverity::HINT, format!("The import specifier can be remapped to \"{to}\" which will resolve it via the active import map."), Some(json!({ "from": from, "to": to }))),
      Self::InvalidAttributeType(assert_type) => (lsp::DiagnosticSeverity::ERROR, format!("The module is a JSON module and expected an attribute type of \"json\". Instead got \"{assert_type}\"."), None),
      Self::NoAttributeType => (lsp::DiagnosticSeverity::ERROR, "The module is a JSON module and not being imported with an import attribute. Consider adding `with { type: \"json\" }` to the import statement.".to_string(), None),
      Self::NoCache(specifier) => (lsp::DiagnosticSeverity::ERROR, format!("Uncached or missing remote URL: {specifier}"), Some(json!({ "specifier": specifier }))),
      Self::NoCacheNpm(pkg_req, specifier) => (lsp::DiagnosticSeverity::ERROR, format!("Uncached or missing npm package: {}", pkg_req), Some(json!({ "specifier": specifier }))),
      Self::NoLocal(specifier) => {
        let sloppy_resolution = SloppyImportsResolver::resolve_with_fs(&deno_fs::RealFs, specifier);
        let data = sloppy_resolution.as_lsp_quick_fix_message().map(|message| {
          json!({
            "specifier": specifier,
            "to": sloppy_resolution.as_specifier(),
            "message": message,
          })
        });
        (lsp::DiagnosticSeverity::ERROR, no_local_message(specifier, sloppy_resolution), data)
      },
      Self::Redirect { from, to} => (lsp::DiagnosticSeverity::INFORMATION, format!("The import of \"{from}\" was redirected to \"{to}\"."), Some(json!({ "specifier": from, "redirect": to }))),
      Self::ResolutionError(err) => (
        lsp::DiagnosticSeverity::ERROR,
        enhanced_resolution_error_message(err),
        graph_util::get_resolution_error_bare_node_specifier(err)
          .map(|specifier| json!({ "specifier": specifier }))
      ),
      Self::InvalidNodeSpecifier(specifier) => (lsp::DiagnosticSeverity::ERROR, format!("Unknown Node built-in module: {}", specifier.path()), None),
      Self::BareNodeSpecifier(specifier) => (lsp::DiagnosticSeverity::WARNING, format!("\"{}\" is resolved to \"node:{}\". If you want to use a built-in Node module, add a \"node:\" prefix.", specifier, specifier), Some(json!({ "specifier": specifier }))),
    };
    lsp::Diagnostic {
      range: *range,
      severity: Some(severity),
      code: Some(lsp::NumberOrString::String(self.code().to_string())),
      source: Some(DiagnosticSource::Deno.as_lsp_source().to_string()),
      message,
      data,
      ..Default::default()
    }
  }
}

fn specifier_text_for_redirected(
  redirect: &lsp::Url,
  referrer: &lsp::Url,
) -> String {
  if redirect.scheme() == "file" && referrer.scheme() == "file" {
    // use a relative specifier when it's going to a file url
    relative_specifier(redirect, referrer)
  } else {
    redirect.to_string()
  }
}

fn relative_specifier(specifier: &lsp::Url, referrer: &lsp::Url) -> String {
  match referrer.make_relative(specifier) {
    Some(relative) => {
      if relative.starts_with('.') {
        relative
      } else {
        format!("./{}", relative)
      }
    }
    None => specifier.to_string(),
  }
}

fn diagnose_resolution(
  snapshot: &language_server::StateSnapshot,
  dependency_key: &str,
  resolution: &Resolution,
  is_dynamic: bool,
  maybe_assert_type: Option<&str>,
) -> Vec<DenoDiagnostic> {
  let mut diagnostics = vec![];
  match resolution {
    Resolution::Ok(resolved) => {
      let specifier = &resolved.specifier;
      // If the module is a remote module and has a `X-Deno-Warning` header, we
      // want a warning diagnostic with that message.
      if let Some(metadata) = snapshot.cache_metadata.get(specifier) {
        if let Some(message) =
          metadata.get(&cache::MetadataKey::Warning).cloned()
        {
          diagnostics.push(DenoDiagnostic::DenoWarn(message));
        }
      }
      if let Some(doc) = snapshot.documents.get(specifier) {
        let doc_specifier = doc.specifier();
        // If the module was redirected, we want to issue an informational
        // diagnostic that indicates this. This then allows us to issue a code
        // action to replace the specifier with the final redirected one.
        if doc_specifier != specifier {
          diagnostics.push(DenoDiagnostic::Redirect {
            from: specifier.clone(),
            to: doc_specifier.clone(),
          });
        }
        if doc.media_type() == MediaType::Json {
          match maybe_assert_type {
            // The module has the correct assertion type, no diagnostic
            Some("json") => (),
            // The dynamic import statement is missing an attribute type, which
            // we might not be able to statically detect, therefore we will
            // not provide a potentially incorrect diagnostic.
            None if is_dynamic => (),
            // The module has an incorrect assertion type, diagnostic
            Some(assert_type) => diagnostics.push(
              DenoDiagnostic::InvalidAttributeType(assert_type.to_string()),
            ),
            // The module is missing an attribute type, diagnostic
            None => diagnostics.push(DenoDiagnostic::NoAttributeType),
          }
        }
      } else if specifier.scheme() == "jsr" {
        // TODO(nayeemrmn): Check if jsr specifiers are cached.
      } else if let Ok(pkg_ref) =
        NpmPackageReqReference::from_specifier(specifier)
      {
        if let Some(npm_resolver) = snapshot
          .npm
          .as_ref()
          .and_then(|n| n.npm_resolver.as_managed())
        {
          // show diagnostics for npm package references that aren't cached
          let req = pkg_ref.into_inner().req;
          if !npm_resolver.is_pkg_req_folder_cached(&req) {
            diagnostics
              .push(DenoDiagnostic::NoCacheNpm(req, specifier.clone()));
          }
        }
      } else if let Some(module_name) = specifier.as_str().strip_prefix("node:")
      {
        if !deno_node::is_builtin_node_module(module_name) {
          diagnostics
            .push(DenoDiagnostic::InvalidNodeSpecifier(specifier.clone()));
        } else if module_name == dependency_key {
          let mut is_mapped = false;
          if let Some(import_map) = &snapshot.maybe_import_map {
            if let Resolution::Ok(resolved) = &resolution {
              if import_map.resolve(module_name, &resolved.specifier).is_ok() {
                is_mapped = true;
              }
            }
          }
          // show diagnostics for bare node specifiers that aren't mapped by import map
          if !is_mapped {
            diagnostics
              .push(DenoDiagnostic::BareNodeSpecifier(module_name.to_string()));
          }
        } else if let Some(npm_resolver) = snapshot
          .npm
          .as_ref()
          .and_then(|n| n.npm_resolver.as_managed())
        {
          // check that a @types/node package exists in the resolver
          let types_node_req = PackageReq::from_str("@types/node").unwrap();
          if !npm_resolver.is_pkg_req_folder_cached(&types_node_req) {
            diagnostics.push(DenoDiagnostic::NoCacheNpm(
              types_node_req,
              ModuleSpecifier::parse("npm:@types/node").unwrap(),
            ));
          }
        }
      } else {
        // When the document is not available, it means that it cannot be found
        // in the cache or locally on the disk, so we want to issue a diagnostic
        // about that.
        let deno_diagnostic = match specifier.scheme() {
          "file" => DenoDiagnostic::NoLocal(specifier.clone()),
          _ => DenoDiagnostic::NoCache(specifier.clone()),
        };
        diagnostics.push(deno_diagnostic);
      }
    }
    // The specifier resolution resulted in an error, so we want to issue a
    // diagnostic for that.
    Resolution::Err(err) => {
      diagnostics.push(DenoDiagnostic::ResolutionError(*err.clone()))
    }
    _ => (),
  }
  diagnostics
}

/// Generate diagnostics related to a dependency. The dependency is analyzed to
/// determine if it can be remapped to the active import map as well as surface
/// any diagnostics related to the resolved code or type dependency.
fn diagnose_dependency(
  diagnostics: &mut Vec<lsp::Diagnostic>,
  snapshot: &language_server::StateSnapshot,
  referrer: &ModuleSpecifier,
  dependency_key: &str,
  dependency: &deno_graph::Dependency,
) {
  if let Some(npm) = &snapshot.npm {
    if npm.npm_resolver.in_npm_package(referrer) {
      return; // ignore, surface typescript errors instead
    }
  }

  if let Some(import_map) = &snapshot.maybe_import_map {
    if let Resolution::Ok(resolved) = &dependency.maybe_code {
      if let Some(to) = import_map.lookup(&resolved.specifier, referrer) {
        if dependency_key != to {
          diagnostics.push(
            DenoDiagnostic::ImportMapRemap {
              from: dependency_key.to_string(),
              to,
            }
            .to_lsp_diagnostic(&documents::to_lsp_range(&resolved.range)),
          );
        }
      }
    }
  }

  let import_ranges: Vec<_> = dependency
    .imports
    .iter()
    .map(|i| documents::to_lsp_range(&i.range))
    .collect();

  diagnostics.extend(
    diagnose_resolution(
      snapshot,
      dependency_key,
      if dependency.maybe_code.is_none() {
        &dependency.maybe_type
      } else {
        &dependency.maybe_code
      },
      dependency.is_dynamic,
      dependency.maybe_attribute_type.as_deref(),
    )
    .iter()
    .flat_map(|diag| {
      import_ranges
        .iter()
        .map(|range| diag.to_lsp_diagnostic(range))
    }),
  );
  // TODO(nayeemrmn): This is a crude way of detecting `@deno-types` which has
  // a different specifier and therefore needs a separate call to
  // `diagnose_resolution()`. It would be much cleaner if that were modelled as
  // a separate dependency: https://github.com/denoland/deno_graph/issues/247.
  if !dependency.maybe_type.is_none()
    && !dependency
      .imports
      .iter()
      .any(|i| dependency.maybe_type.includes(&i.range.start).is_some())
  {
    let range = match &dependency.maybe_type {
      Resolution::Ok(resolved) => documents::to_lsp_range(&resolved.range),
      Resolution::Err(error) => documents::to_lsp_range(error.range()),
      Resolution::None => unreachable!(),
    };
    diagnostics.extend(
      diagnose_resolution(
        snapshot,
        dependency_key,
        &dependency.maybe_type,
        dependency.is_dynamic,
        dependency.maybe_attribute_type.as_deref(),
      )
      .iter()
      .map(|diag| diag.to_lsp_diagnostic(&range)),
    );
  }
}

/// Generate diagnostics that come from Deno module resolution logic (like
/// dependencies) or other Deno specific diagnostics, like the ability to use
/// an import map to shorten an URL.
fn generate_deno_diagnostics(
  snapshot: &language_server::StateSnapshot,
  config: &ConfigSnapshot,
  token: CancellationToken,
) -> DiagnosticVec {
  let mut diagnostics_vec = Vec::new();

  for document in snapshot
    .documents
    .documents(DocumentsFilter::OpenDiagnosable)
  {
    if token.is_cancelled() {
      break;
    }
    let mut diagnostics = Vec::new();
    let specifier = document.specifier();
    if config.specifier_enabled(specifier) {
      for (dependency_key, dependency) in document.dependencies() {
        diagnose_dependency(
          &mut diagnostics,
          snapshot,
          specifier,
          dependency_key,
          dependency,
        );
      }
    }
    diagnostics_vec.push(DiagnosticRecord {
      specifier: specifier.clone(),
      versioned: VersionedDiagnostics {
        version: document.maybe_lsp_version(),
        diagnostics,
      },
    });
  }

  diagnostics_vec
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cache::GlobalHttpCache;
  use crate::cache::RealDenoCacheEnv;
  use crate::lsp::config::ConfigSnapshot;
  use crate::lsp::config::Settings;
  use crate::lsp::config::WorkspaceSettings;
  use crate::lsp::documents::Documents;
  use crate::lsp::documents::LanguageId;
  use crate::lsp::language_server::StateSnapshot;
  use deno_config::glob::FilePatterns;
  use pretty_assertions::assert_eq;
  use std::path::Path;
  use std::path::PathBuf;
  use std::sync::Arc;
  use test_util::TempDir;

  fn mock_state_snapshot(
    fixtures: &[(&str, &str, i32, LanguageId)],
    location: &Path,
    maybe_import_map: Option<(&str, &str)>,
  ) -> StateSnapshot {
    let cache = Arc::new(GlobalHttpCache::new(
      location.to_path_buf(),
      RealDenoCacheEnv,
    ));
    let mut documents = Documents::new(cache);
    for (specifier, source, version, language_id) in fixtures {
      let specifier =
        resolve_url(specifier).expect("failed to create specifier");
      documents.open(
        specifier.clone(),
        *version,
        *language_id,
        (*source).into(),
      );
    }
    let maybe_import_map = maybe_import_map.map(|(base, json_string)| {
      let base_url = ModuleSpecifier::parse(base).unwrap();
      let result = import_map::parse_from_json(&base_url, json_string).unwrap();
      if !result.diagnostics.is_empty() {
        panic!("unexpected import map diagnostics");
      }
      Arc::new(result.import_map)
    });
    StateSnapshot {
      documents,
      maybe_import_map,
      assets: Default::default(),
      cache_metadata: cache::CacheMetadata::new(Arc::new(
        GlobalHttpCache::new(location.to_path_buf(), RealDenoCacheEnv),
      )),
      config: Default::default(),
      npm: None,
    }
  }

  fn mock_config() -> ConfigSnapshot {
    let root_uri = resolve_url("file:///").unwrap();
    ConfigSnapshot {
      settings: Settings {
        unscoped: WorkspaceSettings {
          enable: Some(true),
          lint: true,
          ..Default::default()
        },
        ..Default::default()
      },
      workspace_folders: vec![(
        root_uri.clone(),
        lsp::WorkspaceFolder {
          uri: root_uri,
          name: "".to_string(),
        },
      )],
      ..Default::default()
    }
  }

  fn setup(
    temp_dir: &TempDir,
    sources: &[(&str, &str, i32, LanguageId)],
    maybe_import_map: Option<(&str, &str)>,
  ) -> (StateSnapshot, PathBuf) {
    let location = temp_dir.path().join("deps").to_path_buf();
    let state_snapshot =
      mock_state_snapshot(sources, &location, maybe_import_map);
    (state_snapshot, location)
  }

  #[tokio::test]
  async fn test_enabled_then_disabled_specifier() {
    let temp_dir = TempDir::new();
    let (snapshot, cache_location) = setup(
      &temp_dir,
      &[(
        "file:///a.ts",
        r#"import * as b from "./b.ts";
let a: any = "a";
let c: number = "a";
"#,
        1,
        LanguageId::TypeScript,
      )],
      None,
    );
    let snapshot = Arc::new(snapshot);
    let cache =
      Arc::new(GlobalHttpCache::new(cache_location, RealDenoCacheEnv));
    let ts_server = TsServer::new(Default::default(), cache);
    ts_server.start(None);
    let lint_options = LintOptions {
      rules: Default::default(),
      files: FilePatterns::new_with_base(temp_dir.path().to_path_buf()),
      reporter_kind: Default::default(),
    };

    // test enabled
    {
      let enabled_config = mock_config();
      let diagnostics = generate_lint_diagnostics(
        &snapshot,
        &enabled_config,
        &lint_options,
        Default::default(),
      );
      assert_eq!(get_diagnostics_for_single(diagnostics).len(), 6);
      let diagnostics = generate_ts_diagnostics(
        snapshot.clone(),
        &enabled_config,
        &ts_server,
        Default::default(),
      )
      .await
      .unwrap();
      assert_eq!(get_diagnostics_for_single(diagnostics).len(), 5);
      let diagnostics = generate_deno_diagnostics(
        &snapshot,
        &enabled_config,
        Default::default(),
      );
      assert_eq!(get_diagnostics_for_single(diagnostics).len(), 1);
    }

    // now test disabled specifier
    {
      let mut disabled_config = mock_config();
      disabled_config.settings.unscoped = WorkspaceSettings {
        enable: Some(false),
        ..Default::default()
      };

      let diagnostics = generate_lint_diagnostics(
        &snapshot,
        &disabled_config,
        &lint_options,
        Default::default(),
      );
      assert_eq!(get_diagnostics_for_single(diagnostics).len(), 0);
      let diagnostics = generate_ts_diagnostics(
        snapshot.clone(),
        &disabled_config,
        &ts_server,
        Default::default(),
      )
      .await
      .unwrap();
      assert_eq!(get_diagnostics_for_single(diagnostics).len(), 0);
      let diagnostics = generate_deno_diagnostics(
        &snapshot,
        &disabled_config,
        Default::default(),
      );
      assert_eq!(get_diagnostics_for_single(diagnostics).len(), 0);
    }
  }

  fn get_diagnostics_for_single(
    diagnostic_vec: DiagnosticVec,
  ) -> Vec<lsp::Diagnostic> {
    assert_eq!(diagnostic_vec.len(), 1);
    diagnostic_vec
      .into_iter()
      .next()
      .unwrap()
      .versioned
      .diagnostics
  }

  #[tokio::test]
  async fn test_deno_diagnostics_with_import_map() {
    let temp_dir = TempDir::new();
    let (snapshot, _) = setup(
      &temp_dir,
      &[
        (
          "file:///std/assert/mod.ts",
          "export function assert() {}",
          1,
          LanguageId::TypeScript,
        ),
        (
          "file:///a/file.ts",
          "import { assert } from \"../std/assert/mod.ts\";\n\nassert();\n",
          1,
          LanguageId::TypeScript,
        ),
      ],
      Some((
        "file:///a/import-map.json",
        r#"{
        "imports": {
          "/~/std/": "../std/"
        }
      }"#,
      )),
    );
    let config = mock_config();
    let token = CancellationToken::new();
    let actual = generate_deno_diagnostics(&snapshot, &config, token);
    assert_eq!(actual.len(), 2);
    for record in actual {
      match record.specifier.as_str() {
        "file:///std/assert/mod.ts" => {
          assert_eq!(json!(record.versioned.diagnostics), json!([]))
        }
        "file:///a/file.ts" => assert_eq!(
          json!(record.versioned.diagnostics),
          json!([
            {
              "range": {
                "start": {
                  "line": 0,
                  "character": 23
                },
                "end": {
                  "line": 0,
                  "character": 45
                }
              },
              "severity": 4,
              "code": "import-map-remap",
              "source": "deno",
              "message": "The import specifier can be remapped to \"/~/std/assert/mod.ts\" which will resolve it via the active import map.",
              "data": {
                "from": "../std/assert/mod.ts",
                "to": "/~/std/assert/mod.ts"
              }
            }
          ])
        ),
        _ => unreachable!("unexpected specifier {}", record.specifier),
      }
    }
  }

  #[test]
  fn test_get_code_action_import_map_remap() {
    let specifier = ModuleSpecifier::parse("file:///a/file.ts").unwrap();
    let result = DenoDiagnostic::get_code_action(&specifier, &lsp::Diagnostic {
      range: lsp::Range {
        start: lsp::Position { line: 0, character: 23 },
        end: lsp::Position { line: 0, character: 50 },
      },
      severity: Some(lsp::DiagnosticSeverity::HINT),
      code: Some(lsp::NumberOrString::String("import-map-remap".to_string())),
      source: Some("deno".to_string()),
      message: "The import specifier can be remapped to \"/~/std/assert/mod.ts\" which will resolve it via the active import map.".to_string(),
      data: Some(json!({
        "from": "../std/assert/mod.ts",
        "to": "/~/std/assert/mod.ts"
      })),
      ..Default::default()
    });
    assert!(result.is_ok());
    let actual = result.unwrap();
    assert_eq!(
      json!(actual),
      json!({
        "title": "Update \"../std/assert/mod.ts\" to \"/~/std/assert/mod.ts\" to use import map.",
        "kind": "quickfix",
        "diagnostics": [
          {
            "range": {
              "start": {
                "line": 0,
                "character": 23
              },
              "end": {
                "line": 0,
                "character": 50
              }
            },
            "severity": 4,
            "code": "import-map-remap",
            "source": "deno",
            "message": "The import specifier can be remapped to \"/~/std/assert/mod.ts\" which will resolve it via the active import map.",
            "data": {
              "from": "../std/assert/mod.ts",
              "to": "/~/std/assert/mod.ts"
            }
          }
        ],
        "edit": {
          "changes": {
            "file:///a/file.ts": [
              {
                "range": {
                  "start": {
                    "line": 0,
                    "character": 23
                  },
                  "end": {
                    "line": 0,
                    "character": 50
                  }
                },
                "newText": "\"/~/std/assert/mod.ts\""
              }
            ]
          }
        }
      })
    );
  }

  #[tokio::test]
  async fn duplicate_diagnostics_for_duplicate_imports() {
    let temp_dir = TempDir::new();
    let (snapshot, _) = setup(
      &temp_dir,
      &[(
        "file:///a.ts",
        r#"
        // @deno-types="bad.d.ts"
        import "bad.js";
        import "bad.js";
        "#,
        1,
        LanguageId::TypeScript,
      )],
      None,
    );
    let config = mock_config();
    let token = CancellationToken::new();
    let actual = generate_deno_diagnostics(&snapshot, &config, token);
    assert_eq!(actual.len(), 1);
    let record = actual.first().unwrap();
    assert_eq!(
      json!(record.versioned.diagnostics),
      json!([
        {
          "range": {
            "start": {
              "line": 2,
              "character": 15
            },
            "end": {
              "line": 2,
              "character": 23
            }
          },
          "severity": 1,
          "code": "import-prefix-missing",
          "source": "deno",
          "message": "Relative import path \"bad.js\" not prefixed with / or ./ or ../",
        },
        {
          "range": {
            "start": {
              "line": 3,
              "character": 15
            },
            "end": {
              "line": 3,
              "character": 23
            }
          },
          "severity": 1,
          "code": "import-prefix-missing",
          "source": "deno",
          "message": "Relative import path \"bad.js\" not prefixed with / or ./ or ../",
        },
        {
          "range": {
            "start": {
              "line": 1,
              "character": 23
            },
            "end": {
              "line": 1,
              "character": 33
            }
          },
          "severity": 1,
          "code": "import-prefix-missing",
          "source": "deno",
          "message": "Relative import path \"bad.d.ts\" not prefixed with / or ./ or ../",
        },
      ])
    );
  }

  #[test]
  fn test_specifier_text_for_redirected() {
    #[track_caller]
    fn run_test(specifier: &str, referrer: &str, expected: &str) {
      let result = specifier_text_for_redirected(
        &ModuleSpecifier::parse(specifier).unwrap(),
        &ModuleSpecifier::parse(referrer).unwrap(),
      );
      assert_eq!(result, expected);
    }

    run_test("file:///a/a.ts", "file:///a/mod.ts", "./a.ts");
    run_test("file:///a/a.ts", "file:///a/sub_dir/mod.ts", "../a.ts");
    run_test(
      "file:///a/sub_dir/a.ts",
      "file:///a/mod.ts",
      "./sub_dir/a.ts",
    );
    run_test(
      "https://deno.land/x/example/mod.ts",
      "file:///a/sub_dir/a.ts",
      "https://deno.land/x/example/mod.ts",
    );
  }
}
