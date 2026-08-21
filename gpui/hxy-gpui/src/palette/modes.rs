//! The palette's cascade [`PaletteMode`], its pure-data
//! [`PaletteAction`] vocabulary, and the per-mode entry builders.
//!
//! Everything here is framework-agnostic: [`build_entries`] takes a
//! mode, the raw query, a [`PaletteContext`] snapshot of the active
//! file, and the resolved keybinding hints, and returns the rows the
//! overlay renders. No gpui types leak in, so the builders are unit-
//! tested directly. Argument parsing routes through
//! [`hxy_panels::goto`]; the `@` / `=` calculator prefixes route
//! through [`hxy_calculator`] with a [`NullResolver`] (template field
//! paths arrive in M4).

use std::path::PathBuf;

use hxy_calculator::NullResolver;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_panels::goto::ParseError;
use hxy_panels::goto::parse_count_expr;
use hxy_panels::goto::parse_offset_expr;
use hxy_panels::goto::parse_range_expr;
use hxy_plugin_host::PluginCommand;
use hxy_templates::library::TemplateLibrary;
use palette_core::Entry;

/// The palette's cascade mode. `Main` is the root command list; the
/// rest are single-argument prompt modes reached from it. [`parent`]
/// drives the Escape-pops-back-one-level behaviour.
///
/// [`parent`]: PaletteMode::parent
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    Main,
    GoToOffset,
    /// Virtual-address variant of [`Self::GoToOffset`]. The gpui port
    /// has no virtual-base plumbing yet, so no Main entry constructs it;
    /// the variant exists for cascade parity with the egui app and
    /// parses identically to `GoToOffset` if a future entry surfaces it.
    #[allow(dead_code)]
    GoToAddress,
    SelectFromOffset,
    SelectRange,
    SetColumns,
    /// Second-level cascade shown after the user picks `Run Template...`
    /// from the Main list. Registered templates + install / uninstall /
    /// browse entries. Each pick binds against the whole file. Entries
    /// are built by the overlay (they read the library global and the
    /// active file's head bytes), via [`build_templates_mode_entries`].
    Templates,
    /// Sibling cascade for `Run Template at Selection...`: the same
    /// template list, but each pick binds against the selection the
    /// Main entry advertised. Only reachable when a selection exists.
    TemplatesAtSelection,
    /// Third-level cascade listing installed templates to delete.
    /// Reached from `Main` via "Uninstall template...".
    UninstallTemplate,
    /// First step of the palette-driven compare: pick the A side from
    /// the open files or a disk browse. Entries are built by the overlay
    /// (they depend on the live open-file list), not [`build_entries`].
    CompareSideA,
    /// Pick the B side; the chosen A rides on the overlay's cascade
    /// state until this pick spawns the compare tab.
    CompareSideB,
    /// Sub-menu a plugin returned from an invoke (`InvokeOutcome::Cascade`).
    /// The commands ride on the overlay's `plugin_cascade` state; each
    /// pick re-invokes on the same plugin (egui `enter_plugin_cascade`).
    PluginCascade,
    /// Argument-style prompt a plugin raised (`InvokeOutcome::Prompt`).
    /// The typed answer routes back through `respond_to_prompt` on the
    /// originating (plugin, command id) held in `plugin_prompt` state
    /// (egui `enter_plugin_prompt`).
    PluginPrompt,
}

impl PaletteMode {
    /// One level up the cascade, or `None` at the root. Mirrors the
    /// egui app's `Mode::parent`: every argument mode collapses to
    /// `Main`, and `Main` itself closes the palette outright.
    pub fn parent(self) -> Option<Self> {
        match self {
            PaletteMode::Main => None,
            PaletteMode::GoToOffset
            | PaletteMode::GoToAddress
            | PaletteMode::SelectFromOffset
            | PaletteMode::SelectRange
            | PaletteMode::SetColumns
            | PaletteMode::Templates
            | PaletteMode::TemplatesAtSelection
            | PaletteMode::UninstallTemplate
            | PaletteMode::CompareSideA
            | PaletteMode::CompareSideB
            | PaletteMode::PluginCascade
            | PaletteMode::PluginPrompt => Some(PaletteMode::Main),
        }
    }

    /// Whether this mode treats the query as a raw argument rather than
    /// a fuzzy filter. Argument modes (and the `@` / `=` Main prefixes)
    /// bypass filtering so their single dynamic row is never hidden by
    /// a non-subsequence match against its human-readable label.
    pub fn bypasses_filter(self, query: &str) -> bool {
        match self {
            PaletteMode::Main => {
                let q = query.trim_start();
                q.starts_with('@') || q.starts_with('=')
            }
            // Compare picks, the template lists, and a plugin cascade are
            // fuzzy-filtered lists, not single dynamic argument rows. The
            // plugin prompt (like the arg modes) falls through to `true`:
            // its one answer row must never be filtered away.
            PaletteMode::CompareSideA
            | PaletteMode::CompareSideB
            | PaletteMode::Templates
            | PaletteMode::TemplatesAtSelection
            | PaletteMode::UninstallTemplate
            | PaletteMode::PluginCascade => false,
            _ => true,
        }
    }

    /// The i18n key for this mode's input placeholder / hint.
    pub fn hint_key(self) -> &'static str {
        match self {
            PaletteMode::Main => "palette-hint-main",
            PaletteMode::GoToOffset => "palette-hint-go-to-offset",
            PaletteMode::GoToAddress => "palette-hint-go-to-address",
            PaletteMode::SelectFromOffset => "palette-hint-select-from-offset",
            PaletteMode::SelectRange => "palette-hint-select-range",
            PaletteMode::SetColumns => "palette-hint-set-columns-local",
            PaletteMode::Templates => "palette-hint-templates",
            PaletteMode::TemplatesAtSelection => "palette-hint-templates-at-selection",
            PaletteMode::UninstallTemplate => "palette-hint-uninstall",
            PaletteMode::CompareSideA => "palette-hint-compare-side-a",
            PaletteMode::CompareSideB => "palette-hint-compare-side-b",
            PaletteMode::PluginCascade => "palette-hint-plugin-cascade",
            // Default hint; the overlay overrides it with the plugin's
            // own prompt title, which is the actual question asked.
            PaletteMode::PluginPrompt => "palette-hint-plugin-prompt",
        }
    }
}

/// Which side of a compare pick an entry resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareSide {
    A,
    B,
}

/// Which byte-format a [`PaletteAction::CopySelection`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    /// Space-separated uppercase hex, matching the vim hex-pane yank.
    Hex,
    /// Raw bytes as lossy UTF-8 text, matching the vim ASCII-pane yank.
    Bytes,
}

/// The pure-data outcome of activating an entry. The overlay handles
/// [`Self::SwitchMode`] / [`Self::NoOp`] itself; every other variant is
/// dispatched into the [`Workspace`](crate::workspace::Workspace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    OpenFile,
    CloseTab,
    ToggleVim,
    ToggleInspector,
    /// Open-or-close the cross-file search results tab. Workspace-
    /// scoped (searches every open file), so unlike the file-scoped
    /// entries below it is never gated on `has_active_file`.
    ToggleGlobalSearch,
    /// Open (or focus) the strings panel for the active file, scanning
    /// the whole file under the panel's own auto-run rule.
    OpenStrings,
    /// Open (or focus) the entropy panel for the active file, scanning
    /// the whole file under the panel's own auto-run rule.
    OpenEntropy,
    /// Open (or focus) the checksums panel for the active file, hashing
    /// the whole file under the panel's own auto-run rule.
    OpenChecksums,
    /// Open (or focus) the settings tab. Workspace-scoped like
    /// [`Self::ToggleGlobalSearch`], never gated on `has_active_file`.
    OpenSettings,
    /// Open (or focus) the plugins tab. Workspace-scoped like
    /// [`Self::OpenSettings`], never gated on `has_active_file`.
    OpenPlugins,
    /// Open (or focus) the Console tab. Workspace-scoped like
    /// [`Self::OpenSettings`], never gated on `has_active_file`.
    OpenConsole,
    /// Open (or focus) the visualizer panel for the active file. Only
    /// offered while a template field carries a visualize attribute
    /// (`PaletteContext::visualizer_target_count`), mirroring egui's
    /// `has_visualizer` gate.
    OpenVisualizer,
    /// Mount the active file through its detected VFS handler and swap its
    /// tab for a nested-dock workspace. Only meaningful when the active
    /// file has a detected handler (`PaletteContext::can_browse_vfs`).
    BrowseVfs,
    /// Cascade into an argument mode without closing the palette.
    SwitchMode(PaletteMode),
    /// Move the caret to an absolute offset (relative inputs are
    /// resolved against the cursor before the action is built).
    GoToOffset(u64),
    SetSelection {
        start: u64,
        end_exclusive: u64,
    },
    SetColumns(ColumnCount),
    /// Copy a literal string (the `=<expr>` calculator rows).
    CopyText(String),
    /// Copy the active file's current selection in the given format.
    CopySelection(CopyFormat),
    /// Pick one side of a compare from an already-open file (A advances
    /// the cascade to B; B spawns the compare tab). `from_open_file`
    /// distinguishes an open-file pick (dropped on restore) from a disk
    /// browse pick (disk-restorable); both carry a path.
    CompareSelectSource {
        side: CompareSide,
        path: std::path::PathBuf,
        from_open_file: bool,
    },
    /// Open a file dialog to pick this side from disk.
    CompareBrowse(CompareSide),
    /// Run the template at `path` against the active file: the whole
    /// file when `range` is `None`, the given slice otherwise (baked
    /// from the selection when the entry was built).
    RunTemplate {
        path: PathBuf,
        range: Option<ByteRange>,
    },
    /// Pick a template source from disk with a file dialog and run it
    /// against the whole active file.
    RunTemplateDialog,
    /// Pick a `.bt` from disk and copy it (plus its `#include`
    /// closure) into the user templates directory.
    InstallTemplate,
    /// Delete an installed template source file.
    UninstallTemplate(PathBuf),
    /// Move the caret to the next template field boundary after the
    /// cursor, wrapping to the first field past the end.
    JumpNextField,
    /// Move the caret to the previous template field boundary before
    /// the cursor, wrapping to the last field at the start.
    JumpPrevField,
    /// Download the upstream WerWolv/ImHex-Patterns corpus into the
    /// shared install directory so hundreds more formats auto-detect.
    FetchImhexPatterns,
    /// Invoke a plugin command. The handler is resolved by name at
    /// dispatch; a missing handler logs and is otherwise inert. The
    /// invoke runs off-thread and its outcome may cascade or prompt.
    InvokePluginCommand {
        plugin_name: String,
        command_id: String,
    },
    /// Answer a plugin's [`InvokeOutcome::Prompt`](hxy_plugin_host::InvokeOutcome):
    /// route `answer` back through `respond_to_prompt` on the same
    /// (plugin, command id) the prompt carried.
    RespondToPlugin {
        plugin_name: String,
        command_id: String,
        answer: String,
    },
    /// Inert: placeholder / invalid rows pick to this so a stray Enter
    /// doesn't get the user stuck; the overlay just closes.
    NoOp,
}

/// Snapshot of the active file the builders read to gate and resolve
/// entries. All-false / zero when no file is focused.
#[derive(Clone, Copy, Debug, Default)]
pub struct PaletteContext {
    pub has_active_file: bool,
    pub cursor: u64,
    pub source_len: u64,
    /// `Some((start, end_exclusive))` when a selection exists.
    pub selection: Option<(u64, u64)>,
    pub vim_on: bool,
    /// The active file has a detected VFS handler, so "Browse VFS" would
    /// mount it rather than no-op.
    pub can_browse_vfs: bool,
    /// Number of leaf fields on the active file's active template
    /// instance. Zero when no template has run (or it produced no
    /// fields); the jump next/prev field entries gate on this.
    pub template_field_count: usize,
    /// Number of visualizer-bearing fields across the active file's
    /// template instances. Zero when no template has run or none of
    /// its fields carry a `[[hex::visualize(...)]]` attribute; the
    /// visualizer entry is only listed when nonzero.
    pub visualizer_target_count: usize,
}

/// Resolved keybinding hints for the Main-list commands that mirror a
/// workspace shortcut. `None` when no binding is registered.
#[derive(Clone, Debug, Default)]
pub struct Shortcuts {
    pub open_file: Option<String>,
    pub toggle_vim: Option<String>,
    pub toggle_inspector: Option<String>,
    pub toggle_global_search: Option<String>,
    pub open_settings: Option<String>,
}

/// Ceiling for the palette's column-count input, matching the egui
/// app's slider cap: `ColumnCount` allows up to `u16::MAX`, but wider
/// than this is unreadable at sane font sizes.
const MAX_COLUMNS: u64 = 64;

/// Build the rows for `mode` given the current `query`, active-file
/// `ctx`, and resolved `shortcuts`.
pub fn build_entries(
    mode: PaletteMode,
    query: &str,
    ctx: PaletteContext,
    shortcuts: &Shortcuts,
) -> Vec<Entry<PaletteAction>> {
    let mut out = Vec::new();
    match mode {
        PaletteMode::Main => build_main_entries(&mut out, query, ctx, shortcuts),
        PaletteMode::GoToOffset
        | PaletteMode::GoToAddress
        | PaletteMode::SelectFromOffset
        | PaletteMode::SelectRange
        | PaletteMode::SetColumns => build_arg_entries(&mut out, mode, query.trim(), ctx),
        // Compare picks and the template lists depend on live app
        // state (open files, the library global, the active file's
        // head bytes), which the pure builders don't have; the
        // overlay builds those rows.
        PaletteMode::CompareSideA
        | PaletteMode::CompareSideB
        | PaletteMode::Templates
        | PaletteMode::TemplatesAtSelection
        | PaletteMode::UninstallTemplate
        | PaletteMode::PluginCascade
        | PaletteMode::PluginPrompt => {}
    }
    out
}

/// Rows for the [`PaletteMode::Templates`] /
/// [`PaletteMode::TemplatesAtSelection`] cascades: every library
/// template ranked against the active file, best matches first, each
/// bound to `range` (`None` = whole file). The whole-file list also
/// carries the run-from-disk / install / uninstall management rows
/// (mirrors the egui Templates mode).
pub fn build_templates_mode_entries(
    library: &TemplateLibrary,
    extension: Option<&str>,
    head_bytes: &[u8],
    range: Option<ByteRange>,
) -> Vec<Entry<PaletteAction>> {
    let mut out = Vec::new();
    for entry in library.rank_entries(extension, head_bytes) {
        out.push(
            Entry::new(
                hxy_i18n::t_args("palette-run-template-fmt", &[("name", &entry.name)]),
                PaletteAction::RunTemplate { path: entry.path.clone(), range },
            )
            .with_subtitle(entry.path.display().to_string()),
        );
    }
    if range.is_none() {
        out.push(Entry::new(hxy_i18n::t("gpui-palette-run-template-browse"), PaletteAction::RunTemplateDialog));
        out.push(
            Entry::new(hxy_i18n::t("palette-install-template"), PaletteAction::InstallTemplate)
                .with_subtitle(hxy_i18n::t("palette-install-template-subtitle")),
        );
        out.push(Entry::new(
            hxy_i18n::t("palette-uninstall-template"),
            PaletteAction::SwitchMode(PaletteMode::UninstallTemplate),
        ));
    }
    out
}

/// Rows for the [`PaletteMode::UninstallTemplate`] cascade: one
/// delete row per installed template source file.
pub fn build_uninstall_entries(installed: &[PathBuf]) -> Vec<Entry<PaletteAction>> {
    installed
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            Entry::new(
                hxy_i18n::t_args("palette-delete-template-fmt", &[("name", &name)]),
                PaletteAction::UninstallTemplate(path.clone()),
            )
            .with_subtitle(path.display().to_string())
        })
        .collect()
}

/// The plugin sub-menu backing [`PaletteMode::PluginCascade`]: the
/// plugin whose invoke produced it plus the commands it returned, held
/// so the cascade renders without re-asking the plugin every frame
/// (egui `PluginCascadeState`).
#[derive(Clone)]
pub struct PluginCascadeState {
    pub plugin_name: String,
    pub commands: Vec<PluginCommand>,
}

/// The pending question backing [`PaletteMode::PluginPrompt`]: which
/// (plugin, command id) to answer via `respond_to_prompt` and the
/// title shown as the input hint (egui `PluginPromptState`).
#[derive(Clone)]
pub struct PluginPromptState {
    pub plugin_name: String,
    pub command_id: String,
    pub title: String,
}

/// Main-list rows contributed by loaded plugins: one per command each
/// handler advertises, labeled `"{plugin}: {label}"` and bound to
/// [`PaletteAction::InvokePluginCommand`]. `plugins` pairs each plugin
/// name with its `list_commands()` result (empty without the `commands`
/// grant, so an ungranted plugin adds nothing -- egui parity). Label
/// and subtitle are plugin-authored and pass through untranslated.
pub fn build_plugin_main_entries(plugins: &[(String, Vec<PluginCommand>)]) -> Vec<Entry<PaletteAction>> {
    let mut out = Vec::new();
    for (plugin_name, commands) in plugins {
        for cmd in commands {
            out.push(plugin_command_entry(plugin_name, cmd, true));
        }
    }
    out
}

/// Rows for [`PaletteMode::PluginCascade`]: the plugin's returned
/// sub-commands. Unprefixed labels -- the cascade already scopes to one
/// plugin -- each re-invoking on that same plugin.
pub fn build_plugin_cascade_entries(plugin_name: &str, commands: &[PluginCommand]) -> Vec<Entry<PaletteAction>> {
    commands.iter().map(|cmd| plugin_command_entry(plugin_name, cmd, false)).collect()
}

/// The single answer row for [`PaletteMode::PluginPrompt`]: submitting
/// it sends the current `query` back to the plugin. An empty query
/// shows a placeholder label but still submits (some plugins accept an
/// empty answer -- egui parity); the prompt title rides as subtitle.
pub fn build_plugin_prompt_entry(prompt: &PluginPromptState, query: &str) -> Vec<Entry<PaletteAction>> {
    let answer = query.to_owned();
    let label = if answer.is_empty() { hxy_i18n::t("palette-plugin-prompt-empty") } else { answer.clone() };
    vec![
        Entry::new(
            label,
            PaletteAction::RespondToPlugin {
                plugin_name: prompt.plugin_name.clone(),
                command_id: prompt.command_id.clone(),
                answer,
            },
        )
        .with_subtitle(prompt.title.clone()),
    ]
}

/// One plugin-command row. `prefixed` picks the Main-list label
/// (`"{plugin}: {label}"`) over the cascade label (bare `label`).
fn plugin_command_entry(plugin_name: &str, cmd: &PluginCommand, prefixed: bool) -> Entry<PaletteAction> {
    let title = if prefixed { format!("{plugin_name}: {}", cmd.label) } else { cmd.label.clone() };
    let mut entry = Entry::new(
        title,
        PaletteAction::InvokePluginCommand { plugin_name: plugin_name.to_owned(), command_id: cmd.id.clone() },
    )
    .with_icon(ICON_PLUGIN);
    if let Some(subtitle) = &cmd.subtitle {
        entry = entry.with_subtitle(subtitle.clone());
    }
    entry
}

fn build_main_entries(out: &mut Vec<Entry<PaletteAction>>, query: &str, ctx: PaletteContext, shortcuts: &Shortcuts) {
    // `@<expr>` jumps to a calculated offset; `=<expr>` copies a
    // calculated value. Either prefix replaces the whole Main list:
    // the user committed to the expression flow, and a fuzzy grab-bag
    // of unrelated commands underneath would be noise.
    let trimmed = query.trim_start();
    if let Some(rest) = trimmed.strip_prefix('@') {
        build_calculator_goto(out, rest, ctx);
        return;
    }
    if let Some(rest) = trimmed.strip_prefix('=') {
        build_calculator_copy(out, rest);
        return;
    }

    let mut open = Entry::new(hxy_i18n::t("toolbar-open-file"), PaletteAction::OpenFile);
    if let Some(hint) = &shortcuts.open_file {
        open = open.with_shortcut(hint.clone());
    }
    out.push(open);

    out.push(
        Entry::new(hxy_i18n::t("gpui-palette-close-tab"), PaletteAction::CloseTab).with_disabled(!ctx.has_active_file),
    );

    let mut toggle_vim = Entry::new(hxy_i18n::t("palette-toggle-vim"), PaletteAction::ToggleVim).with_subtitle(
        hxy_i18n::t(if ctx.vim_on { "palette-toggle-vim-subtitle-on" } else { "palette-toggle-vim-subtitle-off" }),
    );
    if let Some(hint) = &shortcuts.toggle_vim {
        toggle_vim = toggle_vim.with_shortcut(hint.clone());
    }
    out.push(toggle_vim);

    let mut toggle_inspector = Entry::new(hxy_i18n::t("gpui-palette-toggle-inspector"), PaletteAction::ToggleInspector);
    if let Some(hint) = &shortcuts.toggle_inspector {
        toggle_inspector = toggle_inspector.with_shortcut(hint.clone());
    }
    out.push(toggle_inspector);

    let mut toggle_global_search =
        Entry::new(hxy_i18n::t("gpui-palette-toggle-global-search"), PaletteAction::ToggleGlobalSearch);
    if let Some(hint) = &shortcuts.toggle_global_search {
        toggle_global_search = toggle_global_search.with_shortcut(hint.clone());
    }
    out.push(toggle_global_search);

    // Always the "show" label (egui flips to "Close Settings" when the
    // tab is open; the gpui action open-or-focuses instead of toggling).
    let mut open_settings = Entry::new(hxy_i18n::t("palette-tool-show-settings"), PaletteAction::OpenSettings);
    if let Some(hint) = &shortcuts.open_settings {
        open_settings = open_settings.with_shortcut(hint.clone());
    }
    out.push(open_settings);

    // Workspace-scoped like settings; egui opens the plugins tab from a
    // menu with no shortcut, so no keybinding hint is surfaced here.
    out.push(Entry::new(hxy_i18n::t("gpui-palette-show-plugins"), PaletteAction::OpenPlugins));

    // Workspace-scoped like settings; egui's Toggle Console has no
    // shortcut, so no keybinding hint is surfaced here.
    out.push(Entry::new(hxy_i18n::t("gpui-palette-show-console"), PaletteAction::OpenConsole));

    out.push(
        Entry::new(hxy_i18n::t("palette-strings-whole-file"), PaletteAction::OpenStrings)
            .with_subtitle(hxy_i18n::t("palette-strings-whole-file-subtitle"))
            .with_disabled(!ctx.has_active_file),
    );

    out.push(
        Entry::new(hxy_i18n::t("palette-compute-entropy"), PaletteAction::OpenEntropy)
            .with_subtitle(hxy_i18n::t("palette-compute-entropy-subtitle"))
            .with_disabled(!ctx.has_active_file),
    );

    out.push(
        Entry::new(hxy_i18n::t("palette-checksums-whole-file"), PaletteAction::OpenChecksums)
            .with_subtitle(hxy_i18n::t("palette-checksums-whole-file-subtitle"))
            .with_disabled(!ctx.has_active_file),
    );

    // Only listed while a template field carries a visualize
    // attribute (mirrors egui: the entry is absent, not disabled,
    // without targets).
    if ctx.has_active_file && ctx.visualizer_target_count > 0 {
        out.push(Entry::new(hxy_i18n::t("palette-tool-show-visualizer"), PaletteAction::OpenVisualizer));
    }

    // Surfaced whether or not it applies (mirrors the egui "Browse VFS"
    // entry) so the command is discoverable; disabled with a reason when
    // the active file has no detected handler.
    let mut browse_vfs =
        Entry::new(hxy_i18n::t("gpui-palette-browse-vfs"), PaletteAction::BrowseVfs).with_disabled(!ctx.can_browse_vfs);
    if !ctx.can_browse_vfs {
        browse_vfs = browse_vfs.with_subtitle(hxy_i18n::t("gpui-palette-browse-vfs-unavailable"));
    }
    out.push(browse_vfs);

    out.push(
        Entry::new(hxy_i18n::t("palette-run-template-entry"), PaletteAction::SwitchMode(PaletteMode::Templates))
            .with_disabled(!ctx.has_active_file),
    );
    // Mirrors egui: the at-selection cascade is only offered while a
    // selection exists (its subtitle advertises the bound range), not
    // rendered disabled.
    if let Some((start, end)) = ctx.selection {
        out.push(
            Entry::new(
                hxy_i18n::t("palette-run-template-at-selection-entry"),
                PaletteAction::SwitchMode(PaletteMode::TemplatesAtSelection),
            )
            .with_subtitle(hxy_i18n::t_args(
                "palette-run-template-at-selection-subtitle",
                &[("start", &format!("{start:#x}")), ("end", &format!("{end:#x}"))],
            )),
        );
    }
    out.push(Entry::new(
        hxy_i18n::t("palette-uninstall-template"),
        PaletteAction::SwitchMode(PaletteMode::UninstallTemplate),
    ));
    // Workspace-scoped like the compare entry: downloading the corpus
    // needs no active file.
    out.push(
        Entry::new(hxy_i18n::t("gpui-palette-fetch-imhex-patterns"), PaletteAction::FetchImhexPatterns)
            .with_subtitle(hxy_i18n::t("gpui-palette-fetch-imhex-patterns-subtitle")),
    );
    let has_fields = ctx.template_field_count > 0;
    for (key, action) in [
        ("palette-jump-next-field", PaletteAction::JumpNextField),
        ("palette-jump-prev-field", PaletteAction::JumpPrevField),
    ] {
        let mut entry = Entry::new(hxy_i18n::t(key), action).with_disabled(!ctx.has_active_file || !has_fields);
        if !has_fields {
            entry = entry.with_subtitle(hxy_i18n::t("palette-jump-field-no-template"));
        }
        out.push(entry);
    }

    out.push(
        Entry::new(hxy_i18n::t("palette-go-to-offset-entry"), PaletteAction::SwitchMode(PaletteMode::GoToOffset))
            .with_disabled(!ctx.has_active_file),
    );
    out.push(
        Entry::new(
            hxy_i18n::t("palette-select-from-offset-entry"),
            PaletteAction::SwitchMode(PaletteMode::SelectFromOffset),
        )
        .with_disabled(!ctx.has_active_file),
    );
    out.push(
        Entry::new(hxy_i18n::t("palette-select-range-entry"), PaletteAction::SwitchMode(PaletteMode::SelectRange))
            .with_disabled(!ctx.has_active_file),
    );
    out.push(
        Entry::new(hxy_i18n::t("palette-set-columns-local-entry"), PaletteAction::SwitchMode(PaletteMode::SetColumns))
            .with_disabled(!ctx.has_active_file),
    );

    // Compare is workspace-scoped (its own two editors), so it needs no
    // active file: even with nothing open the user can browse two files.
    out.push(
        Entry::new(hxy_i18n::t("palette-compare-files"), PaletteAction::SwitchMode(PaletteMode::CompareSideA))
            .with_subtitle(hxy_i18n::t("palette-compare-files-subtitle")),
    );

    let has_selection = ctx.selection.is_some();
    for (key, format) in
        [("gpui-palette-copy-selection-hex", CopyFormat::Hex), ("gpui-palette-copy-selection-bytes", CopyFormat::Bytes)]
    {
        let mut entry =
            Entry::new(hxy_i18n::t(key), PaletteAction::CopySelection(format)).with_disabled(!has_selection);
        if !has_selection {
            entry = entry.with_subtitle(hxy_i18n::t("gpui-palette-copy-selection-none"));
        }
        out.push(entry);
    }
}

/// Resolve a `@<expr>` query into a single Go-to-offset entry. Empty
/// expression renders an inert prompt; parse / evaluation / bounds
/// failures render one disabled "Invalid: ..." row.
fn build_calculator_goto(out: &mut Vec<Entry<PaletteAction>>, expr: &str, ctx: PaletteContext) {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        out.push(Entry::new(hxy_i18n::t("palette-go-to-offset-prompt"), PaletteAction::NoOp));
        return;
    }
    if !ctx.has_active_file {
        push_invalid(out, trimmed, &hxy_i18n::t("palette-invalid-no-active-file"));
        return;
    }
    let value = match hxy_calculator::evaluate_str_with(trimmed, &NullResolver) {
        Ok(v) => v,
        Err(e) => return push_invalid(out, trimmed, &e.to_string()),
    };
    let max_offset = ctx.source_len.saturating_sub(1);
    let target = match value.as_u64() {
        Ok(t) if t <= max_offset => t,
        Ok(t) => {
            let reason = hxy_i18n::t_args(
                "palette-calculator-out-of-range",
                &[("value", &format!("0x{t:X}")), ("max", &format!("0x{max_offset:X}"))],
            );
            return push_invalid(out, trimmed, &reason);
        }
        Err(e) => return push_invalid(out, trimmed, &e.to_string()),
    };
    out.push(
        Entry::new(
            hxy_i18n::t_args("palette-go-to-offset-fmt", &[("offset", &format!("0x{target:X}"))]),
            PaletteAction::GoToOffset(target),
        )
        .with_subtitle(format!("{trimmed} = {}", value.raw())),
    );
}

/// Resolve a `=<expr>` query into decimal + hex "Copy result" rows.
fn build_calculator_copy(out: &mut Vec<Entry<PaletteAction>>, expr: &str) {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        out.push(Entry::new(hxy_i18n::t("palette-copy-result-prompt"), PaletteAction::NoOp));
        return;
    }
    let value = match hxy_calculator::evaluate_str_with(trimmed, &NullResolver) {
        Ok(v) => v,
        Err(e) => return push_invalid(out, trimmed, &e.to_string()),
    };
    let raw = value.raw();
    let decimal = raw.to_string();
    let hex = format_signed_hex(raw);
    out.push(
        Entry::new(
            hxy_i18n::t_args("palette-copy-decimal-fmt", &[("value", &decimal)]),
            PaletteAction::CopyText(decimal.clone()),
        )
        .with_subtitle(hex.clone()),
    );
    out.push(
        Entry::new(hxy_i18n::t_args("palette-copy-hex-fmt", &[("value", &hex)]), PaletteAction::CopyText(hex))
            .with_subtitle(decimal),
    );
}

fn build_arg_entries(out: &mut Vec<Entry<PaletteAction>>, mode: PaletteMode, query: &str, ctx: PaletteContext) {
    if query.is_empty() {
        return;
    }
    if !ctx.has_active_file {
        push_invalid(out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
        return;
    }
    match mode {
        PaletteMode::GoToOffset | PaletteMode::GoToAddress => {
            match parse_offset_expr(query, &NullResolver)
                .and_then(|n| n.resolve(ctx.cursor, ctx.source_len).ok_or(ParseError::OutOfRange))
            {
                Ok(target) => out.push(
                    Entry::new(
                        hxy_i18n::t_args("palette-go-to-offset-fmt", &[("offset", &format!("0x{target:X}"))]),
                        PaletteAction::GoToOffset(target),
                    )
                    .with_subtitle(format!("{target}")),
                ),
                Err(e) => push_invalid(out, query, &e.to_string()),
            }
        }
        PaletteMode::SelectFromOffset => match parse_count_expr(query, &NullResolver) {
            Ok(0) => push_invalid(out, query, &hxy_i18n::t("gpui-palette-invalid-nonzero")),
            Ok(count) => {
                let start = ctx.cursor;
                let available = ctx.source_len.saturating_sub(start);
                if available == 0 {
                    return push_invalid(out, query, &hxy_i18n::t("gpui-palette-invalid-at-eof"));
                }
                let end_exclusive = start + count.min(available);
                out.push(
                    Entry::new(
                        hxy_i18n::t_args(
                            "palette-select-from-offset-fmt",
                            &[("count", &(end_exclusive - start).to_string()), ("start", &format!("0x{start:X}"))],
                        ),
                        PaletteAction::SetSelection { start, end_exclusive },
                    )
                    .with_subtitle(format!("0x{start:X} .. 0x{end_exclusive:X}")),
                );
            }
            Err(e) => push_invalid(out, query, &e.to_string()),
        },
        PaletteMode::SelectRange => match parse_range_expr(query, ctx.source_len, &NullResolver) {
            Ok(range) => out.push(Entry::new(
                hxy_i18n::t_args(
                    "palette-select-range-fmt",
                    &[
                        ("start", &format!("0x{:X}", range.start)),
                        ("end", &format!("0x{:X}", range.end_exclusive)),
                        ("count", &range.len().to_string()),
                    ],
                ),
                PaletteAction::SetSelection { start: range.start, end_exclusive: range.end_exclusive },
            )),
            Err(e) => push_invalid(out, query, &e.to_string()),
        },
        PaletteMode::SetColumns => match parse_count_expr(query, &NullResolver) {
            Ok(n) if (1..=MAX_COLUMNS).contains(&n) => {
                // `n <= 64` fits `u16`, so `new` only fails on zero,
                // already excluded by the range check.
                match ColumnCount::new(n as u16) {
                    Ok(count) => out.push(Entry::new(
                        hxy_i18n::t_args("palette-set-columns-local-fmt", &[("count", &n.to_string())]),
                        PaletteAction::SetColumns(count),
                    )),
                    Err(e) => push_invalid(out, query, &e.to_string()),
                }
            }
            Ok(_) => push_invalid(
                out,
                query,
                &hxy_i18n::t_args("palette-invalid-columns-range", &[("max", &MAX_COLUMNS.to_string())]),
            ),
            Err(e) => push_invalid(out, query, &e.to_string()),
        },
        // Not arg modes: `build_entries` never routes them here.
        PaletteMode::Main
        | PaletteMode::Templates
        | PaletteMode::TemplatesAtSelection
        | PaletteMode::UninstallTemplate
        | PaletteMode::CompareSideA
        | PaletteMode::CompareSideB
        | PaletteMode::PluginCascade
        | PaletteMode::PluginPrompt => {}
    }
}

/// Semantic icon token for warning/invalid rows. The overlay maps it
/// to a rendered icon; kept a plain string so this module stays free
/// of UI-framework types.
pub const ICON_WARNING: &str = "warning";

/// Semantic icon token for plugin-contributed command rows. The overlay
/// maps it to a puzzle-piece glyph, mirroring the egui palette's
/// `icon::PUZZLE_PIECE` on plugin commands. A plugin-supplied
/// `PluginCommand::icon` (a phosphor codepoint) is not carried through:
/// the gpui icon set is a fixed vendored asset enum, so every plugin
/// command shows the puzzle-piece fallback rather than a custom glyph.
pub const ICON_PLUGIN: &str = "plugin";

/// Push a disabled "Invalid: {reason}" row bound to [`PaletteAction::NoOp`].
fn push_invalid(out: &mut Vec<Entry<PaletteAction>>, query: &str, reason: &str) {
    out.push(
        Entry::new(hxy_i18n::t_args("palette-invalid-fmt", &[("reason", reason)]), PaletteAction::NoOp)
            .with_subtitle(query.to_owned())
            .with_icon(ICON_WARNING)
            .with_disabled(true),
    );
}

/// Format a signed `i128` as a `0x...` literal, signed-magnitude:
/// `-16` renders `-0x10`, not a two's-complement pattern. Mirrors the
/// egui app's `format_signed_hex` so paste targets read the same.
fn format_signed_hex(value: i128) -> String {
    if value < 0 { format!("-0x{:X}", value.unsigned_abs()) } else { format!("0x{value:X}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_ctx() -> PaletteContext {
        PaletteContext {
            has_active_file: true,
            cursor: 0,
            source_len: 256,
            selection: None,
            vim_on: false,
            can_browse_vfs: true,
            template_field_count: 0,
            visualizer_target_count: 0,
        }
    }

    fn actions(entries: &[Entry<PaletteAction>]) -> Vec<PaletteAction> {
        entries.iter().map(|e| e.data.clone()).collect()
    }

    #[test]
    fn parent_cascade_collapses_to_main_then_none() {
        assert_eq!(PaletteMode::GoToOffset.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::SelectRange.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::SetColumns.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::Main.parent(), None);
    }

    #[test]
    fn main_list_has_the_core_commands_and_gates_on_active_file() {
        let entries = build_entries(PaletteMode::Main, "", active_ctx(), &Shortcuts::default());
        let data = actions(&entries);
        assert!(data.contains(&PaletteAction::OpenFile));
        assert!(data.contains(&PaletteAction::CloseTab));
        assert!(data.contains(&PaletteAction::ToggleVim));
        assert!(data.contains(&PaletteAction::ToggleInspector));
        assert!(data.contains(&PaletteAction::OpenStrings));
        assert!(data.contains(&PaletteAction::OpenEntropy));
        assert!(data.contains(&PaletteAction::OpenChecksums));
        assert!(data.contains(&PaletteAction::OpenSettings));
        assert!(data.contains(&PaletteAction::OpenPlugins));
        assert!(data.contains(&PaletteAction::OpenConsole));
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::GoToOffset)));
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::SetColumns)));
        // Every row enabled when a file is active (copy needs a
        // selection and the field jumps need a template run, so those
        // four are the exceptions).
        let disabled: Vec<_> = entries.iter().filter(|e| e.disabled).map(|e| e.data.clone()).collect();
        assert_eq!(
            disabled,
            vec![
                PaletteAction::JumpNextField,
                PaletteAction::JumpPrevField,
                PaletteAction::CopySelection(CopyFormat::Hex),
                PaletteAction::CopySelection(CopyFormat::Bytes),
            ]
        );
    }

    #[test]
    fn browse_vfs_entry_gates_on_a_detected_handler() {
        // Present and enabled when a handler was detected.
        let entries = build_entries(PaletteMode::Main, "", active_ctx(), &Shortcuts::default());
        let browse = entries.iter().find(|e| e.data == PaletteAction::BrowseVfs).expect("browse vfs row present");
        assert!(!browse.disabled, "enabled when the file has a detected handler");

        // Present but disabled (with a reason) when none was detected.
        let mut ctx = active_ctx();
        ctx.can_browse_vfs = false;
        let entries = build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default());
        let browse = entries.iter().find(|e| e.data == PaletteAction::BrowseVfs).expect("browse vfs row present");
        assert!(browse.disabled, "disabled when no handler matches the file");
        assert!(browse.subtitle.is_some(), "shows a reason when disabled");
    }

    /// The visualizer entry mirrors egui's `has_visualizer` gate: it
    /// is absent (not disabled) until a template field carries a
    /// visualize attribute.
    #[test]
    fn visualizer_entry_listed_only_with_targets() {
        let data = actions(&build_entries(PaletteMode::Main, "", active_ctx(), &Shortcuts::default()));
        assert!(!data.contains(&PaletteAction::OpenVisualizer), "no targets, no row");

        let mut ctx = active_ctx();
        ctx.visualizer_target_count = 2;
        let data = actions(&build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default()));
        assert!(data.contains(&PaletteAction::OpenVisualizer));

        // Never listed without an active file, targets or not.
        let ctx = PaletteContext { visualizer_target_count: 2, ..PaletteContext::default() };
        let data = actions(&build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default()));
        assert!(!data.contains(&PaletteAction::OpenVisualizer));
    }

    #[test]
    fn main_list_disables_file_commands_with_no_active_file() {
        let ctx = PaletteContext::default();
        let entries = build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default());
        let find = |a: &PaletteAction| entries.iter().find(|e| &e.data == a).expect("row present");
        assert!(find(&PaletteAction::CloseTab).disabled);
        assert!(find(&PaletteAction::OpenStrings).disabled);
        assert!(find(&PaletteAction::OpenEntropy).disabled);
        assert!(find(&PaletteAction::OpenChecksums).disabled);
        assert!(find(&PaletteAction::SwitchMode(PaletteMode::GoToOffset)).disabled);
        assert!(find(&PaletteAction::CopySelection(CopyFormat::Hex)).disabled);
        // Open File / Toggle Vim / Toggle Inspector stay enabled.
        assert!(!find(&PaletteAction::OpenFile).disabled);
        assert!(!find(&PaletteAction::ToggleVim).disabled);
    }

    #[test]
    fn copy_entries_enabled_when_selection_present() {
        let mut ctx = active_ctx();
        ctx.selection = Some((4, 8));
        let entries = build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default());
        let hex = entries.iter().find(|e| e.data == PaletteAction::CopySelection(CopyFormat::Hex)).unwrap();
        assert!(!hex.disabled);
    }

    #[test]
    fn shortcut_hints_ride_the_entries_when_supplied() {
        let shortcuts = Shortcuts { open_file: Some("cmd-o".into()), ..Shortcuts::default() };
        let entries = build_entries(PaletteMode::Main, "", active_ctx(), &shortcuts);
        let open = entries.iter().find(|e| e.data == PaletteAction::OpenFile).unwrap();
        assert_eq!(open.shortcut.as_deref(), Some("cmd-o"));
    }

    #[test]
    fn go_to_offset_relative_resolves_against_cursor() {
        let mut ctx = active_ctx();
        ctx.cursor = 0x10;
        let entries = build_entries(PaletteMode::GoToOffset, "+10", ctx, &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::GoToOffset(0x1A)]);
        assert!(!entries[0].disabled);
    }

    #[test]
    fn go_to_offset_absolute_and_hex() {
        let entries = build_entries(PaletteMode::GoToOffset, "0x20", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::GoToOffset(0x20)]);
    }

    #[test]
    fn go_to_offset_invalid_renders_one_disabled_row() {
        let entries = build_entries(PaletteMode::GoToOffset, "not-a-number", active_ctx(), &Shortcuts::default());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].disabled);
        assert_eq!(entries[0].data, PaletteAction::NoOp);
        assert_eq!(entries[0].icon.as_deref(), Some(ICON_WARNING), "invalid rows carry the warning icon token");
    }

    #[test]
    fn select_from_offset_clamps_to_available_bytes() {
        let mut ctx = active_ctx();
        ctx.cursor = 250;
        ctx.source_len = 256;
        let entries = build_entries(PaletteMode::SelectFromOffset, "100", ctx, &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::SetSelection { start: 250, end_exclusive: 256 }]);
    }

    #[test]
    fn select_range_parses_start_end() {
        let entries = build_entries(PaletteMode::SelectRange, "0x10..0x20", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::SetSelection { start: 0x10, end_exclusive: 0x20 }]);
    }

    #[test]
    fn set_columns_valid_and_out_of_range() {
        let ok = build_entries(PaletteMode::SetColumns, "24", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&ok), vec![PaletteAction::SetColumns(ColumnCount::new(24).unwrap())]);
        let bad = build_entries(PaletteMode::SetColumns, "999", active_ctx(), &Shortcuts::default());
        assert_eq!(bad.len(), 1);
        assert!(bad[0].disabled);
    }

    #[test]
    fn calculator_at_prefix_builds_a_goto_row() {
        let entries = build_entries(PaletteMode::Main, "@0x10 + 0x10", active_ctx(), &Shortcuts::default());
        assert_eq!(actions(&entries), vec![PaletteAction::GoToOffset(0x20)]);
    }

    #[test]
    fn calculator_at_out_of_range_is_disabled() {
        let mut ctx = active_ctx();
        ctx.source_len = 16;
        let entries = build_entries(PaletteMode::Main, "@0x1000", ctx, &Shortcuts::default());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].disabled);
    }

    #[test]
    fn calculator_equals_builds_decimal_and_hex_copy_rows() {
        let entries = build_entries(PaletteMode::Main, "=2+2", active_ctx(), &Shortcuts::default());
        assert_eq!(
            actions(&entries),
            vec![PaletteAction::CopyText("4".into()), PaletteAction::CopyText("0x4".into()),]
        );
    }

    #[test]
    fn bypass_filter_engages_for_arg_modes_and_calc_prefixes() {
        assert!(PaletteMode::GoToOffset.bypasses_filter(""));
        assert!(PaletteMode::Main.bypasses_filter("@0x10"));
        assert!(PaletteMode::Main.bypasses_filter("=2+2"));
        assert!(!PaletteMode::Main.bypasses_filter("open"));
        assert!(!PaletteMode::Templates.bypasses_filter("png"));
        assert!(!PaletteMode::UninstallTemplate.bypasses_filter("png"));
    }

    #[test]
    fn template_modes_cascade_from_main() {
        assert_eq!(PaletteMode::Templates.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::TemplatesAtSelection.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::UninstallTemplate.parent(), Some(PaletteMode::Main));
    }

    #[test]
    fn main_list_offers_template_cascades_and_field_jumps() {
        let entries = build_entries(PaletteMode::Main, "", active_ctx(), &Shortcuts::default());
        let data = actions(&entries);
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::Templates)));
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::UninstallTemplate)));
        assert!(data.contains(&PaletteAction::FetchImhexPatterns));
        // No selection: the at-selection cascade is omitted, not disabled.
        assert!(!data.contains(&PaletteAction::SwitchMode(PaletteMode::TemplatesAtSelection)));
        // No template run: the jump entries are present but disabled.
        let next = entries.iter().find(|e| e.data == PaletteAction::JumpNextField).unwrap();
        assert!(next.disabled);
        assert!(next.subtitle.is_some(), "explains why it is disabled");

        let mut ctx = active_ctx();
        ctx.selection = Some((4, 8));
        ctx.template_field_count = 3;
        let entries = build_entries(PaletteMode::Main, "", ctx, &Shortcuts::default());
        let data = actions(&entries);
        assert!(data.contains(&PaletteAction::SwitchMode(PaletteMode::TemplatesAtSelection)));
        let next = entries.iter().find(|e| e.data == PaletteAction::JumpNextField).unwrap();
        assert!(!next.disabled, "field jumps enable once a template has fields");
    }

    /// Build a library of two templates in a temp dir: one that
    /// magic-matches PNG heads, one for ZIP extensions.
    fn library_fixture() -> (tempfile::TempDir, TemplateLibrary) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("png.bt"), "// ID Bytes: 89 50 4E 47\nstruct P { int x; };\n").unwrap();
        std::fs::write(dir.path().join("zip.bt"), "// File Mask: *.zip\nstruct Z { int x; };\n").unwrap();
        let library = TemplateLibrary::load_from(Some(dir.path()));
        (dir, library)
    }

    #[test]
    fn templates_mode_ranks_matches_first_and_adds_management_rows() {
        let (_dir, library) = library_fixture();
        let entries = build_templates_mode_entries(&library, None, &[0x89, 0x50, 0x4E, 0x47], None);
        // Magic hit floats to the top.
        assert!(
            matches!(&entries[0].data, PaletteAction::RunTemplate { path, range: None } if path.ends_with("png.bt"))
        );
        assert!(
            matches!(&entries[1].data, PaletteAction::RunTemplate { path, range: None } if path.ends_with("zip.bt"))
        );
        let tail = actions(&entries[2..]);
        assert_eq!(
            tail,
            vec![
                PaletteAction::RunTemplateDialog,
                PaletteAction::InstallTemplate,
                PaletteAction::SwitchMode(PaletteMode::UninstallTemplate),
            ]
        );
    }

    #[test]
    fn templates_at_selection_binds_the_range_and_drops_management_rows() {
        let (_dir, library) = library_fixture();
        let range = ByteRange::new(hxy_core::ByteOffset::new(4), hxy_core::ByteOffset::new(8)).unwrap();
        let entries = build_templates_mode_entries(&library, Some("zip"), &[], Some(range));
        assert_eq!(entries.len(), 2, "only run rows in the at-selection cascade");
        assert!(matches!(&entries[0].data, PaletteAction::RunTemplate { path, range: Some(r) }
            if path.ends_with("zip.bt") && *r == range));
    }

    #[test]
    fn uninstall_entries_list_installed_templates() {
        let installed = vec![PathBuf::from("/tmp/templates/png.bt"), PathBuf::from("/tmp/templates/zip.hexpat")];
        let entries = build_uninstall_entries(&installed);
        assert_eq!(
            actions(&entries),
            vec![
                PaletteAction::UninstallTemplate(installed[0].clone()),
                PaletteAction::UninstallTemplate(installed[1].clone()),
            ]
        );
        assert!(entries[0].title.contains("png.bt"));
    }

    fn cmd(id: &str, label: &str, subtitle: Option<&str>) -> PluginCommand {
        PluginCommand {
            id: id.to_owned(),
            label: label.to_owned(),
            subtitle: subtitle.map(str::to_owned),
            icon: None,
            has_children: false,
        }
    }

    #[test]
    fn plugin_modes_cascade_from_main() {
        assert_eq!(PaletteMode::PluginCascade.parent(), Some(PaletteMode::Main));
        assert_eq!(PaletteMode::PluginPrompt.parent(), Some(PaletteMode::Main));
        // The cascade is a fuzzy list; the prompt is a single dynamic row.
        assert!(!PaletteMode::PluginCascade.bypasses_filter("con"));
        assert!(PaletteMode::PluginPrompt.bypasses_filter("anything"));
    }

    #[test]
    fn plugin_main_entries_prefix_the_plugin_name_and_bind_the_command() {
        let plugins = vec![(
            "xeedee".to_owned(),
            vec![cmd("connect", "Connect", Some("open a session")), cmd("list", "List mounts", None)],
        )];
        let entries = build_plugin_main_entries(&plugins);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "xeedee: Connect");
        assert_eq!(entries[0].subtitle.as_deref(), Some("open a session"));
        assert_eq!(
            entries[0].data,
            PaletteAction::InvokePluginCommand { plugin_name: "xeedee".into(), command_id: "connect".into() }
        );
        assert_eq!(entries[1].title, "xeedee: List mounts");
        assert!(entries[1].subtitle.is_none());
        // Both rows carry the puzzle-piece token (egui's PUZZLE_PIECE).
        assert_eq!(entries[0].icon.as_deref(), Some(ICON_PLUGIN));
        assert_eq!(entries[1].icon.as_deref(), Some(ICON_PLUGIN));
    }

    #[test]
    fn plugin_main_entries_empty_without_commands() {
        assert!(build_plugin_main_entries(&[]).is_empty());
        assert!(build_plugin_main_entries(&[("p".to_owned(), vec![])]).is_empty());
    }

    #[test]
    fn plugin_cascade_entries_drop_the_prefix_and_reinvoke_on_the_plugin() {
        let cmds = vec![cmd("a", "Alpha", None), cmd("b", "Beta", Some("second"))];
        let entries = build_plugin_cascade_entries("demo", &cmds);
        assert_eq!(entries[0].title, "Alpha", "cascade rows are unprefixed");
        assert_eq!(
            entries[1].data,
            PaletteAction::InvokePluginCommand { plugin_name: "demo".into(), command_id: "b".into() }
        );
        assert_eq!(entries[1].subtitle.as_deref(), Some("second"));
        // Cascade rows carry the puzzle-piece token like the Main list.
        assert_eq!(entries[0].icon.as_deref(), Some(ICON_PLUGIN));
    }

    #[test]
    fn plugin_prompt_entry_bakes_the_answer_and_shows_the_title() {
        let state =
            PluginPromptState { plugin_name: "demo".into(), command_id: "prompt".into(), title: "Token name".into() };
        let filled = build_plugin_prompt_entry(&state, "session-1");
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].title, "session-1");
        assert_eq!(filled[0].subtitle.as_deref(), Some("Token name"));
        assert_eq!(
            filled[0].data,
            PaletteAction::RespondToPlugin {
                plugin_name: "demo".into(),
                command_id: "prompt".into(),
                answer: "session-1".into(),
            }
        );

        // Empty query: placeholder label, empty answer, still one row.
        let empty = build_plugin_prompt_entry(&state, "");
        assert_ne!(empty[0].title, "", "empty answer shows a placeholder label");
        assert_eq!(
            empty[0].data,
            PaletteAction::RespondToPlugin {
                plugin_name: "demo".into(),
                command_id: "prompt".into(),
                answer: String::new(),
            }
        );
    }
}
