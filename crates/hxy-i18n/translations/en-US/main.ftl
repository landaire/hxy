# App-wide
app-name = hxy
app-tagline = A hex editor

# Menus
menu-file = File
menu-file-open = Open…
menu-file-save = Save
menu-file-save-as = Save As…
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
menu-view-console = Show Console
menu-view-inspector = Show Inspector
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

welcome-recent = Recent files
welcome-recent-empty = No recent files yet.

toolbar-open-file = Open file…
toolbar-browse-archive = Browse archive
toolbar-run-template = Run template…
settings-offset-base = Offset base

# Command palette
palette-hint-main = Search commands, files, templates…
palette-hint-templates = Filter templates…
palette-hint-uninstall = Uninstall which template?
palette-run-template-entry = Run Template…
palette-run-template-fmt = Run { $name }
palette-install-template = Install template…
palette-install-template-subtitle = Pick a .bt file; any #included dependencies come with it.
palette-uninstall-template = Uninstall template…
palette-delete-template-fmt = Delete { $name }

# Errors
error-open-failed = Failed to open file
error-read-failed = Failed to read file

# Duplicate-open dialog
duplicate-open-title = File already open
duplicate-open-body = This file is already open in another tab.
duplicate-open-focus = Go to existing tab
duplicate-open-new-tab = Open in new tab
duplicate-open-cancel = Cancel

# Status bar
status-lock-readonly-tooltip = Read-only -- click to enable edits
status-lock-mutable-tooltip = Editable -- click to lock

# Restore-unsaved-edits dialog
restore-patch-title = Restore unsaved edits?
restore-patch-body = { $ops } unsaved edit(s) from a previous session were saved alongside this file.
restore-patch-restore = Restore
restore-patch-restore-anyway = Restore anyway
restore-patch-discard = Discard
restore-patch-warn-modified = The file has changed on disk since these edits were saved. Restoring may land them at the wrong offsets.
restore-patch-warn-unknown = Unable to confirm the file matches what the edits were saved against.
