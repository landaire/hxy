# App-wide
app-name = hxy
app-tagline = A hex editor

# Menus
menu-file = File
menu-file-new = New
menu-file-open = Open...
menu-file-save = Save
menu-file-save-as = Save As...
menu-file-close = Close
menu-file-quit = Quit
menu-edit = Edit
menu-edit-undo = Undo
menu-edit-redo = Redo
menu-edit-enter-edit-mode = Enter Edit Mode
menu-edit-leave-edit-mode = Leave Edit Mode
menu-edit-copy-bytes = Copy bytes
menu-edit-copy-hex = Copy hex string
menu-edit-paste = Paste
menu-edit-paste-as-hex = Paste as hex
menu-view = View
menu-view-console = Toggle Console
menu-view-inspector = Toggle Inspector
menu-view-plugins = Toggle Plugins
palette-subtitle-show = Show
palette-subtitle-hide = Hide
menu-help = Help
menu-help-about = About

# Tabs
tab-settings = Settings
tab-welcome = Welcome
tab-console = Console
tab-inspector = Inspector

# Console tab
console-empty = No plugin output yet. Template diagnostics will appear here.

# Settings panel
settings-general-header = General
settings-language = Language
settings-language-system = System default
settings-zoom = Zoom
settings-zoom-reset = Reset
settings-columns = Hex columns
settings-check-updates = Check for updates
settings-byte-highlight = Byte-value highlighting
settings-byte-highlight-mode = Highlight mode
settings-byte-highlight-background = Background
settings-byte-highlight-text = Text
settings-byte-highlight-scheme = Highlight scheme
settings-byte-highlight-scheme-class = By class
settings-byte-highlight-scheme-value = By value
settings-minimap = Show minimap
settings-minimap-colored = Colored minimap
settings-input-mode = Input bindings
settings-input-mode-default = Standard
settings-input-mode-vim = Vim

welcome-recent = Recent files
welcome-recent-empty = No recent files yet.

toolbar-open-file = Open file...
toolbar-browse-vfs = Browse VFS
palette-browse-vfs-unavailable = no VFS handler for this file
toolbar-run-template = Run template...
settings-offset-base = Offset base
settings-address-separator = Group hex digits in address column
settings-address-separator-char = Address group separator
palette-plugin-prompt-empty = (type an answer)

# Command palette
palette-hint-main = Search commands, files, templates...
palette-hint-quick-open = Switch to open file...
palette-hint-templates = Filter templates...
palette-hint-uninstall = Uninstall which template?
palette-hint-recent = Reopen which recent file?
palette-open-recent-entry = Open recent...
palette-hint-go-to-offset = Offset (decimal, 0x..., +N, -N)
palette-hint-select-from-offset = Byte count (decimal, 0x...)
palette-hint-select-range = start, end  (one endpoint may be +/- relative)
palette-hint-set-columns-local = Hex columns for this buffer (1..64)
palette-hint-set-columns-global = Hex columns (default for all buffers, 1..64)
palette-go-to-offset-entry = Go to offset...
palette-jump-next-field = Jump to next template field
palette-jump-prev-field = Jump to previous template field
palette-jump-field-no-template = no template loaded
palette-select-from-offset-entry = Select bytes from current offset...
palette-select-range-entry = Select range...
palette-go-to-offset-fmt = Go to offset { $offset }
palette-select-from-offset-fmt = Select { $count } bytes from { $start }
palette-select-range-fmt = Select { $start } .. { $end } ({ $count } bytes)
palette-invalid-fmt = Invalid: { $reason }
palette-invalid-no-active-file = no active file
palette-invalid-columns-range = column count must be 1..{ $max }
palette-set-columns-local-entry = Set hex columns (this buffer)...
palette-set-columns-global-entry = Set hex columns (default)...
palette-set-columns-local-fmt = Use { $count } columns for this buffer
palette-set-columns-global-fmt = Set { $count } columns as the global default
palette-copy-caret-offset = Copy caret offset
palette-copy-selection-range = Copy selection range
palette-copy-selection-length = Copy selection length
palette-copy-file-length = Copy file length
palette-toggle-readonly = Toggle Readonly
palette-toggle-readonly-result-mutable = Mutable
palette-toggle-readonly-result-readonly = Readonly
palette-run-template-entry = Run Template...
palette-run-template-fmt = Run { $name }
palette-install-template = Install template...
palette-install-template-subtitle = Pick a .bt file; any #included dependencies come with it.
palette-uninstall-template = Uninstall template...
palette-delete-template-fmt = Delete { $name }
palette-uninstall-plugin = Uninstall plugin...
palette-hint-uninstall-plugin = Uninstall which plugin?
palette-delete-plugin-fmt = Uninstall { $name }
palette-delete-plugin-subtitle = Removes the .wasm + manifest, drops the user grant, and clears persisted state.
palette-split-right = Split pane right
palette-split-left = Split pane left
palette-split-up = Split pane up
palette-split-down = Split pane down
palette-merge-right = Merge pane with right
palette-merge-left = Merge pane with left
palette-merge-up = Merge pane with up
palette-merge-down = Merge pane with down
palette-move-tab-right = Move tab right
palette-move-tab-left = Move tab left
palette-move-tab-up = Move tab up
palette-move-tab-down = Move tab down
palette-move-tab-visual = Move tab to pane...
palette-merge-visual = Merge pane into...
palette-focus-pane = Focus pane...
palette-pane-pick-subtitle = Press the highlighted letter on the target pane
palette-toggle-vim = Toggle Vim mode
palette-toggle-vim-subtitle-on = Currently ON -- switch back to standard input
palette-toggle-vim-subtitle-off = Currently OFF -- switch to modal editing

# File comparison
tab-compare-title = Compare: { $a } / { $b }
compare-recompute = Recompute diff
compare-status = { $a } vs { $b } -- { $changes } change(s)
compare-status-pending = Diff pending
compare-status-recomputing = Computing diff...
compare-no-differences = No differences
compare-table-kind = Kind
compare-table-a-range = A
compare-table-b-range = B
compare-table-size = Size
compare-table-size-fmt = a={ $a } b={ $b }
compare-kind-added = Added
compare-kind-removed = Removed
compare-kind-changed = Changed
compare-diff-colors-toggle = Diff colors
compare-picker-title = Compare files
compare-picker-body = Pick the A and B sources to compare. Open files appear in the dropdown; "Browse..." reads from disk.
compare-picker-side-a = A:
compare-picker-side-b = B:
compare-picker-unset = (pick a file)
compare-picker-section-open = Open files
compare-picker-section-recent = Recent files
compare-picker-browse = Browse...
compare-picker-confirm = Compare
compare-picker-cancel = Cancel
palette-compare-files = Compare files...
palette-compare-files-subtitle = Pick A then B from open files, recents, or disk
palette-compare-files-dialog = Compare files (dialog)...
palette-compare-files-dialog-subtitle = Open a modal picker with combo boxes for both sides
palette-hint-compare-side-a = Pick the A side to compare...
palette-hint-compare-side-b = Pick the B side to compare against A...
compare-sync-scroll = Sync scroll
compare-sync-scroll-tooltip = Keep both panes scrolled to the same offset
compare-deadline-label = Diff budget
compare-deadline-tooltip = Maximum time the Myers diff may run before falling back to an approximation. Edit to override the global default for this comparison.
compare-deadline-reset = Reset
compare-deadline-reset-tooltip = Drop the per-comparison override and follow the global default
settings-compare-deadline = Compare diff budget
settings-compare-deadline-tooltip = Default time budget for compare-tab Myers diffs. Individual comparisons can override this on their toolbar.

# Search bar
search-find-label = Find:
search-replace-label = Replace:
search-kind-text = Text
search-kind-hex-bytes = Hex Bytes
search-kind-number = Number
search-number-width = { $bits }-bit
search-signed = signed
search-endian-little = LE
search-endian-big = BE
search-next-tooltip = Next match (Enter)
search-prev-tooltip = Previous match (Shift+Enter)
search-all-results = All
search-scope-in-selection = in selection
search-scope-in-selection-tooltip = Search restricted to the current selection -- click to clear
search-replace-toggle-show = + Replace
search-replace-toggle-hide = - Replace
search-replace-toggle-tooltip = Show / hide the replace input
search-close-tooltip = Close (Esc)
search-status-active-of-total = { $index }/{ $total }
search-status-match-count = { $count } matches
search-status-press-enter = Enter to find
search-replace-once = Replace
search-replace-once-tooltip = Replace the current match and advance to the next
search-replace-all = Replace All
search-replace-all-tooltip = Replace every match in the current scope
search-wrapped-forward = Search wrapped to top
search-wrapped-backward = Search wrapped to bottom
search-replace-prompt-title = Replacement changes file size
search-replace-prompt-body = The replacement is { $repl-len } byte(s); the find pattern is { $find-len } byte(s). Continuing will splice every match, changing the file's length.
search-replace-prompt-confirm = Continue
search-replace-prompt-cancel = Cancel
search-replace-all-confirm-title = Replace all matches
search-replace-all-confirm-body = Replace { $count } occurrence(s) of the find pattern with the replacement?
search-replace-all-confirm-yes = Replace All
search-replace-all-confirm-no = Cancel
search-replaced-toast = Replaced { $count } match(es)

# Errors
error-open-failed = Failed to open file
error-read-failed = Failed to read file

# Duplicate-open dialog
duplicate-open-title = File already open
duplicate-open-body = This file is already open in another tab.
duplicate-open-focus = Go to existing tab
duplicate-open-new-tab = Open in new tab
duplicate-open-cancel = Cancel

# Close-with-unsaved-changes dialog
close-prompt-title = Save before closing?
close-prompt-body = { $name } has unsaved changes. Your changes will be lost if you don't save them.
close-prompt-save = Save
close-prompt-discard = Don't Save
close-prompt-cancel = Cancel

# Status bar
status-lock-readonly-tooltip = Read-only -- click to enable edits
status-lock-mutable-tooltip = Editable -- click to lock
# Tooltip on the lock icon when the buffer is hard-readonly: the user
# can't toggle to mutable. `$reason` is the reason text (one of the
# `readonly-reason-*` strings below).
status-lock-readonly-locked-tooltip = Read-only -- {$reason}
readonly-reason-vfs-no-writer = the backing VFS doesn't support writes

# Restore-unsaved-edits dialog
restore-patch-title = Restore unsaved edits?
restore-patch-body = { $ops } unsaved edit(s) from a previous session were saved alongside this file.
restore-patch-restore = Restore
restore-patch-restore-anyway = Restore anyway
restore-patch-discard = Discard
restore-patch-warn-modified = The file has changed on disk since these edits were saved. Restoring may land them at the wrong offsets.
restore-patch-warn-unknown = Unable to confirm the file matches what the edits were saved against.

# ImHex-Patterns download flow
patterns-prompt-title = Download ImHex pattern library?
patterns-prompt-body = hxy can run patterns from the upstream WerWolv/ImHex-Patterns repository. Downloading them lets the auto-detector recognize hundreds more file formats out of the box.
patterns-prompt-disclaimer = If a downloaded pattern misbehaves, please verify the issue reproduces in ImHex itself before reporting it to the ImHex-Patterns project -- the bug may be in our interpreter rather than the template.
patterns-prompt-download = Download now
patterns-prompt-not-now = Not now
patterns-prompt-dont-ask = Don't ask again
patterns-fetch-started = Downloading ImHex patterns...
patterns-fetch-done = ImHex patterns ready.
patterns-fetch-failed = ImHex patterns download failed: { $error }
patterns-fetch-no-data-dir = No application data directory available; cannot install ImHex patterns.
patterns-settings-title = ImHex pattern library
patterns-settings-not-installed = Not installed.
patterns-settings-installed = Installed (hash { $hash }).
patterns-settings-download-now = Download now
patterns-settings-update = Check for updates
patterns-settings-downloading = Downloading... { $bytes } bytes received

# Open-file template prompts
toast-template-suggestion = Run { $name }?
toast-template-run = Run
toast-template-dismiss = Dismiss

# Reload-on-disk-change dialog
reload-prompt-title = File changed on disk
reload-prompt-body-modified = { $name } was modified outside hxy. Reload from disk?
reload-prompt-body-removed = { $name } was removed from disk.
reload-prompt-warn-unsaved = This buffer has unsaved edits. "Reload" discards them; "Keep edits" replays your patch on top of the new contents (undo history is dropped either way).
reload-prompt-remember = Always do this for this file
reload-prompt-discard = Reload (discard edits)
reload-prompt-discard-tooltip = Replace the in-memory bytes with the new disk contents. Drops the patch and undo history.
reload-prompt-keep = Keep my edits
reload-prompt-keep-tooltip = Refresh the base bytes from disk while keeping your splices applied on top. Undo history is cleared.
reload-prompt-ignore = Ignore this change
reload-prompt-ignore-tooltip = Leave the in-memory bytes as they are; the on-disk drift is acknowledged but not applied.
reload-prompt-cancel = Decide later
palette-reload-file = Reload file from disk...
palette-reload-file-subtitle = Re-read the active tab's bytes from disk; choose whether to keep your edits.
palette-reload-no-active-file = no active file
palette-reload-no-disk-source = active file isn't backed by a disk path
settings-watch-header = Filesystem watcher
settings-auto-reload = When a file changes on disk
auto-reload-always = Reload automatically
auto-reload-ask = Ask
auto-reload-never = Disable watching
settings-poll-interval = Poll fallback interval
settings-poll-interval-tooltip = Cadence for the polling worker that handles paths the kernel watcher rejected. Set to 0 to disable polling entirely.
settings-poll-all = Poll every watched file
settings-poll-all-tooltip = Poll every watched file in addition to kernel events. Useful on network drives or FUSE mounts where kernel notifications are unreliable.

# Orphaned VFS entry dialog
orphan-entry-title = VFS entry no longer exists
orphan-entry-body = After the reload, { $name } can no longer be located inside its parent mount (path: { $path }). Close the tab, or keep it open with the last-known bytes still visible?
orphan-entry-close = Close tab
orphan-entry-close-tooltip = Drop the orphaned tab; its in-memory bytes are discarded.
orphan-entry-keep = Keep open
orphan-entry-keep-tooltip = Leave the tab open. Writeback through the mount is broken but the bytes can still be inspected and copied.

# Snapshot manager
palette-take-snapshot = Take snapshot
palette-take-snapshot-subtitle = Capture the active tab's current bytes; persisted across restarts.
palette-open-snapshots = Snapshots...
palette-open-snapshots-subtitle = List, rename, delete, or compare snapshots for the active file.
snapshot-dialog-title = Snapshots: { $name }
snapshot-take-label = New snapshot:
snapshot-take-name-hint = Optional name
snapshot-take-button = Take
snapshot-empty = No snapshots yet. Use Take to capture the current bytes.
snapshot-no-store = This buffer doesn't have a stable identity, so snapshots can't be stored for it.
snapshot-rename-hint = Press Enter to commit, Esc to cancel
snapshot-compare-current = Compare with current
snapshot-compare-current-tooltip = Open a Compare tab between this snapshot and the live patched bytes.
snapshot-compare-pair = Compare A vs B
snapshot-delete = Delete
snapshot-delete-tooltip = Remove the snapshot and its sidecar bytes.
snapshot-pair-header = Compare two snapshots
snapshot-pick-empty = (pick a snapshot)
snapshot-pick-current = Current bytes
snapshot-size-cached = { $size } (cached)
snapshot-size-disk = { $size } (on disk)
snapshot-capture-toast = Captured snapshot { $id }

# Per-file watch toggle
palette-watch-always = Watch this file: Always reload
palette-watch-ask = Watch this file: Ask before reloading
palette-watch-never = Watch this file: Disable change detection
palette-watch-subtitle = { $mode }{ $marker }
watch-pref-applied = Set to { $mode } for this file

# Poll-interval palette mode
palette-set-poll-interval-entry = Set poll interval...
palette-set-poll-interval-current = Currently { $ms } ms (0 disables polling)
palette-hint-set-poll-interval = Poll interval in milliseconds (0 to disable)
palette-set-poll-interval-fmt = Use { $ms } ms poll interval
palette-set-poll-interval-clamped = Use { $clamped } ms poll interval ({ $ms } clamped to range)
palette-set-poll-interval-off = Disable polling

# Status-bar watch indicator
status-watch-watching = Monitoring this file for external changes
status-watch-not-watching = Change detection disabled for this file
status-watch-cadence-fs-notify = Kernel notifications + polling fallback every { $ms } ms
status-watch-cadence-fs-notify-only = Kernel notifications only (polling off)
status-watch-cadence-fs-poll = Polling every { $ms } ms
status-watch-cadence-vfs-poll = Polling VFS bytes (sample-hash) every { $ms } ms
status-watch-cadence-off = Polling disabled in settings
status-watch-mode = Mode: { $mode }
status-watch-tooltip-prefix = File watcher
status-watch-tooltip-anonymous = This buffer has no persistent identity, so change detection doesn't apply.

# Tabs
tab-entropy = Entropy

# Entropy panel
entropy-heading = Shannon entropy
entropy-no-active-file = No active file -- focus a file tab to compute entropy.
entropy-empty = No entropy computed yet. Click Compute to scan the file.
entropy-zero-bytes = This buffer is empty.
entropy-compute = Compute
entropy-recompute = Recompute
entropy-computing = Computing...
entropy-summary = mean { $mean } / max { $max } bits/byte over { $count } windows ({ $window } each)

# Palette entries (entropy)
palette-compute-entropy = Compute entropy
palette-compute-entropy-subtitle = Run a Shannon-entropy scan across the active file's bytes.
palette-show-entropy = Show entropy panel
palette-show-entropy-subtitle = Open the entropy plot for the active file (no recompute).
